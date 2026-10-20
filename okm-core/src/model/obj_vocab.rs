//! Name vocabulary (ADR-0012, ADR-0017 §3): the run-time, per-collection,
//! bidirectional mapping between names and their numbers. Stored as two
//! point-lookup tables inside the collection's ns segment:
//!
//! ```text
//! slot id:   [ns][id_slot][number u16 BE] → name bytes
//! slot name: [ns][name_slot][name bytes]  → number u16 BE
//! ```
//!
//! TWO vocabularies exist, sharing the protocol but never an id space:
//!
//! - the payload's field names (`FIELD_VOCAB`, fixed faces 0x02/0x03);
//! - the collection's predicate vocabulary (`PREDICATE_VOCAB`, fixed faces 0x04/0x05) —
//!   a graph's predicates, a node collection's kind tokens.
//!
//! The face pair is a construction parameter, not a second allocation
//! algorithm: first-seen-claims-next append-only numbering, the in-memory
//! mirror, and the load-once discipline are identical for both. A sibling
//! type would duplicate that logic and let the two drift apart — the one
//! thing the split exists to prevent is two allocators in one id space.
//!
//! Discipline copied from the ns vocabulary (ADR-0002): append-only —
//! ids are never reused, never renumbered; a name seen for the first
//! time on the write path claims the next free number (single-writer
//! allocation, serialized by the engine mutex — the same boundary a
//! local caller's `&mut self` provides). `u8` ids (`< 0xFF`) are the
//! compact lane; `0xFF` escapes to `[0xFF][u16]` so growth never
//! requires a migration.
//!
//! The cache mirrors both directions in memory. Consistency rests on
//! the single-writer discipline; if several processes share one engine,
//! the cache degrades to connection lifetime (reload on reconnect) —
//! acceptable for a vocabulary that only grows.

use crate::engine::storage::{scan_suffix_kv, MemBatch, VirtualStorage};
use crate::model::index::{vocab_faces, FIELD_VOCAB_ID_SLOT, FIELD_VOCAB_NAME_SLOT, PREDICATE_VOCAB_ID_SLOT, PREDICATE_VOCAB_NAME_SLOT, Slot, write_slot};
use std::collections::HashMap;

/// The pair of faces one vocabulary occupies. Numbers are structural
/// (ADR-0030): a vocabulary is a fixed face pair, never a band inside
/// another pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VocabFaces {
    /// number → name
    pub id_slot: Slot,
    /// name → number
    pub name_slot: Slot,
}

/// The data vocabulary of ONE declared `Vocab<T>` field, picked by that
/// field's declaration-order index among the document's `Vocab` fields
/// (ADR-0032). Per field, not per collection: two categorical fields are two
/// independent domains — independent id space, independent 255-value budget,
/// and each field's names are enumerable on their own (one prefix scan of its
/// id face).
pub const fn data_vocab(field_index: u16) -> VocabFaces {
    let (id_slot, name_slot) = vocab_faces(field_index);
    VocabFaces { id_slot, name_slot }
}
/// Field names of the payload, in the document's own ns (ADR-0012).
pub const FIELD_VOCAB: VocabFaces = VocabFaces { id_slot: FIELD_VOCAB_ID_SLOT, name_slot: FIELD_VOCAB_NAME_SLOT };
/// The collection's predicate vocabulary: a graph's predicates, a node
/// collection's kind tokens (ADR-0017 §3).
pub const PREDICATE_VOCAB: VocabFaces =
    VocabFaces { id_slot: PREDICATE_VOCAB_ID_SLOT, name_slot: PREDICATE_VOCAB_NAME_SLOT };

/// One vocabulary cache, bound to one ns segment and one face pair. Both
/// directions plus the allocation cursor; lazily loaded from the engine on
/// first touch.
#[derive(Debug)]
pub struct VocabCache {
    faces: VocabFaces,
    by_id: HashMap<u16, String>,
    by_name: HashMap<String, u16>,
    next_id: u16,
    loaded: bool,
}

impl VocabCache {
    pub fn new(faces: VocabFaces) -> Self {
        Self {
            faces,
            by_id: HashMap::new(),
            by_name: HashMap::new(),
            next_id: 0,
            loaded: false,
        }
    }

    /// Look up (or allocate) the id for `name` against the engine.
    /// `header` = the collection's key header (`[ns]`) — vocabulary keys
    /// live inside the same ns segment as the data they name.
    pub fn id_for<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &mut S,
        header: &[u8],
        name: &str,
    ) -> u16 {
        self.load(store, header);
        if let Some(id) = self.by_name.get(name) {
            return *id;
        }
        let id = self.next_id;
        self.next_id += 1;
        // One engine batch writes both directions: the vocabulary is
        // consistent even if the caller's own write fails later (extra
        // entries are harmless — numbers are append-only).
        let mut batch = MemBatch::default();
        let mut id_key = header.to_vec();
        write_slot(&mut id_key, self.faces.id_slot);
        id_key.extend_from_slice(&id.to_be_bytes());
        batch.put(id_key, name.as_bytes().to_vec());
        let mut name_key = header.to_vec();
        write_slot(&mut name_key, self.faces.name_slot);
        name_key.extend_from_slice(name.as_bytes());
        batch.put(name_key, id.to_be_bytes().to_vec());
        let _ = store.commit_batch(batch);
        self.by_id.insert(id, name.to_owned());
        self.by_name.insert(name.to_owned(), id);
        id
    }

    /// Resolve ids from decoded frames to names. Unknown ids (a writer
    /// added entries this cache has not loaded — impossible under
    /// single-writer, possible with shared engines) fall back to a
    /// direct engine probe, then to `None` (caller renders a placeholder
    /// or drops the field).
    pub fn name_for<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &S,
        header: &[u8],
        id: u16,
    ) -> Option<String> {
        self.load(store, header);
        if let Some(n) = self.by_id.get(&id) {
            return Some(n.clone());
        }
        // Direct probe before giving up (shared-engine catch-up).
        let mut key = header.to_vec();
        write_slot(&mut key, self.faces.id_slot);
        key.extend_from_slice(&id.to_be_bytes());
        let name = store.get(&key)?;
        let name = String::from_utf8(name).ok()?;
        self.by_id.insert(id, name.clone());
        self.by_name.insert(name.clone(), id);
        if id >= self.next_id {
            self.next_id = id + 1;
        }
        Some(name)
    }

    /// Non-allocating name → id probe that LOADS first: a handle that never
    /// wrote anything still answers (the discipline "prime the cache before
    /// a typed read" is retired — the read itself primes).
    pub fn id_get<S: VirtualStorage + ?Sized>(&mut self, store: &S, header: &[u8], name: &str) -> Option<u16> {
        self.load(store, header);
        self.by_name.get(name).copied()
    }

    /// Load both directions from the engine once. Full-vocabulary load, not
    /// per-id probes: the vocabulary is small and append-only, so one scan
    /// covers every future read until this process writes a new name itself.
    fn load<S: VirtualStorage + ?Sized>(&mut self, store: &S, header: &[u8]) {
        if self.loaded {
            return;
        }
        let mut p2 = header.to_vec();
        write_slot(&mut p2, self.faces.id_slot);
        for (suffix, name) in scan_suffix_kv(store, &p2) {
            if suffix.len() != 2 {
                continue;
            }
            let id = u16::from_be_bytes([suffix[0], suffix[1]]);
            if let Ok(n) = String::from_utf8(name) {
                self.by_id.insert(id, n);
            }
        }
        // The name direction mirrors; also derives next_id (max + 1).
        let mut p3 = header.to_vec();
        write_slot(&mut p3, self.faces.name_slot);
        for (suffix, idv) in scan_suffix_kv(store, &p3) {
            if let Ok(n) = String::from_utf8(suffix)
                && idv.len() == 2 {
                    let id = u16::from_be_bytes([idv[0], idv[1]]);
                    self.by_name.insert(n, id);
                    if id >= self.next_id {
                        self.next_id = id + 1;
                    }
                }
        }
        self.loaded = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_engine::TestStore;

    const HEADER: &[u8] = &[0x00, 0x21]; // ns 33, no partition

    fn key(header: &[u8], slot: Slot, suffix: &[u8]) -> Vec<u8> {
        let mut k = header.to_vec();
        write_slot(&mut k, slot);
        k.extend_from_slice(suffix);
        k
    }

    #[test]
    fn allocates_sequentially_and_is_cached() {
        let mut store = TestStore::slatedb_mem();
        let mut d = VocabCache::new(FIELD_VOCAB);
        let a = d.id_for(&mut store, HEADER, "alpha");
        let b = d.id_for(&mut store, HEADER, "beta");
        let a2 = d.id_for(&mut store, HEADER, "alpha");
        assert_eq!((a, b, a2), (0, 1, 0), "first-seen claims next, repeat hits cache");

        // Both directions landed in the engine.
        assert_eq!(
            store.get(&key(HEADER, FIELD_VOCAB_ID_SLOT, &0u16.to_be_bytes())).unwrap(),
            b"alpha"
        );
        assert_eq!(
            store.get(&key(HEADER, FIELD_VOCAB_NAME_SLOT, b"beta")).unwrap(),
            &1u16.to_be_bytes()
        );
    }

    #[test]
    fn the_two_vocabularies_never_share_an_id_space() {
        let mut store = TestStore::slatedb_mem();
        let mut fields = VocabCache::new(FIELD_VOCAB);
        let mut kinds = VocabCache::new(PREDICATE_VOCAB);
        // The same name allocates in both; the same number names two
        // different things — that is precisely what the split permits.
        assert_eq!(fields.id_for(&mut store, HEADER, "since"), 0);
        assert_eq!(kinds.id_for(&mut store, HEADER, "has"), 0);
        assert_eq!(fields.name_for(&store, HEADER, 0).unwrap(), "since");
        assert_eq!(kinds.name_for(&store, HEADER, 0).unwrap(), "has");
        // …and they land on different faces.
        assert_eq!(store.get(&key(HEADER, PREDICATE_VOCAB_ID_SLOT, &0u16.to_be_bytes())).unwrap(), b"has");
        assert_eq!(store.get(&key(HEADER, FIELD_VOCAB_ID_SLOT, &0u16.to_be_bytes())).unwrap(), b"since");
    }

    #[test]
    fn fresh_cache_loads_from_engine_and_resumes_numbering() {
        let mut store = TestStore::slatedb_mem();
        {
            let mut d = VocabCache::new(PREDICATE_VOCAB);
            d.id_for(&mut store, HEADER, "one");
            d.id_for(&mut store, HEADER, "two");
        }
        // A brand-new cache (process restart) resumes after the max.
        let mut d2 = VocabCache::new(PREDICATE_VOCAB);
        assert_eq!(d2.name_for(&store, HEADER, 0).unwrap(), "one");
        assert_eq!(d2.name_for(&store, HEADER, 1).unwrap(), "two");
        let fresh = d2.id_for(&mut store, HEADER, "three");
        assert_eq!(fresh, 2, "numbering resumes after the loaded max");
        // A read-only handle answers without the caller priming anything.
        let mut d3 = VocabCache::new(PREDICATE_VOCAB);
        assert_eq!(d3.id_get(&store, HEADER, "two"), Some(1));
    }

    #[test]
    fn compact_ids_use_one_byte_and_escape_beyond() {
        let mut store = TestStore::slatedb_mem();
        let mut d = VocabCache::new(FIELD_VOCAB);
        let small = d.id_for(&mut store, HEADER, "small");
        assert!(small < 0xFF);
        // Directly seed an escaped-range name, then read it back.
        let mut batch = MemBatch::default();
        batch.put(key(HEADER, FIELD_VOCAB_ID_SLOT, &0x1234u16.to_be_bytes()), b"big".to_vec());
        batch.put(key(HEADER, FIELD_VOCAB_NAME_SLOT, b"big"), 0x1234u16.to_be_bytes().to_vec());
        let _ = store.commit_batch(batch);
        assert_eq!(d.name_for(&store, HEADER, 0x1234).unwrap(), "big");
    }

    #[test]
    fn data_vocabularies_are_one_pair_per_field_in_family_four() {
        // Per FIELD (ADR-0032): each declared Vocab<T> field owns its own pair,
        // allocated in declaration order inside counting family 4.
        let (id0, name0) = (data_vocab(0).id_slot, data_vocab(0).name_slot);
        let (id1, name1) = (data_vocab(1).id_slot, data_vocab(1).name_slot);
        assert_ne!(id0, id1, "one pair per field");
        assert_eq!(id0 >> 12, 4, "vocabularies live in counting family 4");
        assert_eq!(id1, id0 + 2, "declaration order, two counters per field");
        assert_eq!(name0, id0 + 1);
        assert_eq!(name1, id1 + 1);
        // Two fields no longer share an id space: the same number in two
        // fields names two different values.
        let mut store = TestStore::slatedb_mem();
        let mut status = VocabCache::new(data_vocab(0));
        let mut color = VocabCache::new(data_vocab(1));
        assert_eq!(status.id_for(&mut store, HEADER, "active"), 0);
        assert_eq!(color.id_for(&mut store, HEADER, "red"), 0);
        assert_eq!(status.name_for(&store, HEADER, 0).unwrap(), "active");
        assert_eq!(color.name_for(&store, HEADER, 0).unwrap(), "red");
    }
}