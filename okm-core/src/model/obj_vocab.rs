//! Name vocabularies (ADR-0012, ADR-0017 §3, ADR-0032): the run-time,
//! per-collection bidirectional mapping between a label and its number. Each
//! vocabulary is two point-lookup faces inside the collection's ns:
//!
//! ```text
//! label → id:   [ns][0x1/counter][label bytes]   → id (I bytes BE)
//! id → label:   [ns][0x2/counter][id I bytes BE] → label bytes
//! ```
//!
//! The two directions are two counting FAMILIES (0x1 and 0x2) carrying the SAME
//! counter, so one vocabulary spends one number and its pair cannot be split by
//! an off-by-one: the direction is the family, not a parity convention.
//!
//! THREE vocabularies share the protocol, never an id space:
//!
//! - the payload's field names (`FIELD_VOCAB`, fixed faces 0x02/0x03);
//! - the collection's predicate vocabulary (`PREDICATE_VOCAB`, fixed faces
//!   0x04/0x05) — a graph's predicates, a node collection's kind tokens;
//! - one data vocabulary per declared `Vocab<N>` field (`data_vocab`), in
//!   counting families 0x1/0x2, counters continuing in declaration order.
//!
//! The face pair and the id width are construction parameters, not a second
//! allocation algorithm: first-seen-claims-next append-only numbering, the
//! in-memory mirror, and the load-once discipline are identical everywhere. A
//! sibling type would duplicate that logic and let the copies drift apart — the
//! one thing the split exists to prevent is two allocators in one id space.
//!
//! Discipline copied from the ns vocabulary (ADR-0002): append-only — ids are
//! never reused, never renumbered; a label seen for the first time on the write
//! path claims the next free number (single-writer allocation, serialized by
//! the engine mutex — the same boundary a local caller's `&mut self` provides).
//! Exhausting the declared width is a loud error, never a wrap.
//!
//! The cache mirrors both directions in memory. Consistency rests on the
//! single-writer discipline; if several processes share one engine, the cache
//! degrades to connection lifetime (reload on reconnect) — acceptable for a
//! vocabulary that only grows.

use crate::engine::storage::{scan_suffix_kv, MemBatch, VirtualStorage};
use crate::model::index::{
    segment, slot, write_slot, Slot, FIELD_VOCAB_ID_SLOT, FIELD_VOCAB_NAME_SLOT,
    PREDICATE_VOCAB_ID_SLOT, PREDICATE_VOCAB_NAME_SLOT,
};
use crate::model::vocab::VocabId;
use std::collections::HashMap;

/// The pair of faces one vocabulary occupies. Numbers are structural
/// (ADR-0030): a vocabulary is a pair of counting-family faces, never a band
/// inside another family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VocabFaces {
    /// id → label
    pub id_slot: Slot,
    /// label → id
    pub name_slot: Slot,
}

/// The data vocabulary of ONE declared `Vocab<N>` field, picked by that field's
/// declaration-order index among the document's `Vocab` fields (ADR-0032).
/// Per field, not per collection: two categorical fields are two independent
/// domains — independent id space, independent capacity, and each field's
/// labels are enumerable on their own (one prefix scan of its id face).
pub const fn data_vocab(field_index: u16) -> VocabFaces {
    let counter = 1 + field_index; // counter 0 is reserved (the "unset" look)
    VocabFaces {
        id_slot: slot(segment::VOCAB_ID, counter),
        name_slot: slot(segment::VOCAB_LABEL, counter),
    }
}

impl VocabFaces {
    /// The pair that owns a given id face — for readers that only know the
    /// field's `FieldType::Vocab(id_slot)`: the label direction is the SAME
    /// counter in the other family (ADR-0032).
    pub const fn from_id_slot(id_slot: Slot) -> Self {
        Self {
            id_slot,
            name_slot: slot(segment::VOCAB_LABEL, id_slot & 0x0FFF),
        }
    }
}

/// Point lookup of a label from raw id bytes, for readers that keep no cache
/// (the dynamic path): one engine probe, and `None` when the vocabulary has no
/// such id — a reader never invents a label.
pub fn vocab_label<S: VirtualStorage + ?Sized>(
    store: &S,
    header: &[u8],
    faces: VocabFaces,
    id_be: &[u8],
) -> Option<String> {
    let mut key = header.to_vec();
    write_slot(&mut key, faces.id_slot);
    key.extend_from_slice(id_be);
    String::from_utf8(store.get(&key)?).ok()
}

/// Field names of the payload, in the document's own ns (ADR-0012).
pub const FIELD_VOCAB: VocabFaces =
    VocabFaces { id_slot: FIELD_VOCAB_ID_SLOT, name_slot: FIELD_VOCAB_NAME_SLOT };
/// The collection's predicate vocabulary: a graph's predicates, a node
/// collection's kind tokens (ADR-0017 §3).
pub const PREDICATE_VOCAB: VocabFaces =
    VocabFaces { id_slot: PREDICATE_VOCAB_ID_SLOT, name_slot: PREDICATE_VOCAB_NAME_SLOT };

/// One vocabulary cache, bound to one ns segment, one face pair, and one id
/// width `I`. Both directions plus the allocation cursor; lazily loaded from
/// the engine on first touch.
#[derive(Debug)]
pub struct VocabCache<I: VocabId = u16> {
    faces: VocabFaces,
    by_id: HashMap<I, String>,
    by_name: HashMap<String, I>,
    next_id: I,
    loaded: bool,
}

impl<I: VocabId> VocabCache<I> {
    pub fn new(faces: VocabFaces) -> Self {
        Self {
            faces,
            by_id: HashMap::new(),
            by_name: HashMap::new(),
            next_id: I::ZERO,
            loaded: false,
        }
    }

    /// Look up (or allocate) the id for `label` against the engine.
    /// `header` = the collection's key header (`[ns]`) — vocabulary keys live
    /// inside the same ns segment as the data they name. Exhausting the width
    /// panics: the alternative is a wrapped id naming the wrong label forever.
    pub fn id_for<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &mut S,
        header: &[u8],
        label: &str,
    ) -> I {
        self.load(store, header);
        if let Some(id) = self.by_name.get(label) {
            return *id;
        }
        let id = self.next_id;
        self.next_id = match id.succ() {
            Some(next) => next,
            None => panic!(
                "vocabulary exhausted: {} labels fill a {}-byte id (ADR-0032); this label \
                 does not fit — widen the field's N or model the value as a String field",
                id.as_u128() + 1,
                I::WIDTH
            ),
        };
        // One engine batch writes both directions: the vocabulary is consistent
        // even if the caller's own write fails later (extra entries are
        // harmless — numbers are append-only).
        let mut batch = MemBatch::default();
        let mut name_key = header.to_vec();
        write_slot(&mut name_key, self.faces.name_slot);
        name_key.extend_from_slice(label.as_bytes());
        batch.put(name_key, id.to_be());
        let mut id_key = header.to_vec();
        write_slot(&mut id_key, self.faces.id_slot);
        id_key.extend_from_slice(&id.to_be());
        batch.put(id_key, label.as_bytes().to_vec());
        let _ = store.commit_batch(batch);
        self.by_id.insert(id, label.to_owned());
        self.by_name.insert(label.to_owned(), id);
        id
    }

    /// Resolve ids from decoded frames to labels. Unknown ids (a writer added
    /// entries this cache has not loaded — impossible under single-writer,
    /// possible with shared engines) fall back to a direct engine probe, then to
    /// `None` (the caller leaves the value unresolved or renders a placeholder).
    pub fn name_for<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &S,
        header: &[u8],
        id: I,
    ) -> Option<String> {
        self.load(store, header);
        if let Some(n) = self.by_id.get(&id) {
            return Some(n.clone());
        }
        // Direct probe before giving up (shared-engine catch-up).
        let mut key = header.to_vec();
        write_slot(&mut key, self.faces.id_slot);
        key.extend_from_slice(&id.to_be());
        let name = store.get(&key)?;
        let name = String::from_utf8(name).ok()?;
        self.by_id.insert(id, name.clone());
        self.by_name.insert(name.clone(), id);
        if let Some(next) = id.succ() {
            if next > self.next_id {
                self.next_id = next;
            }
        }
        Some(name)
    }

    /// Non-allocating label → id probe that LOADS first: a handle that never
    /// wrote anything still answers (the discipline "prime the cache before a
    /// typed read" is retired — the read itself primes).
    pub fn id_get<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &S,
        header: &[u8],
        label: &str,
    ) -> Option<I> {
        self.load(store, header);
        self.by_name.get(label).copied()
    }

    /// Load both directions from the engine once. Full-vocabulary load, not
    /// per-id probes: the vocabulary is small and append-only, so one scan
    /// covers every future read until this process writes a new label itself.
    fn load<S: VirtualStorage + ?Sized>(&mut self, store: &S, header: &[u8]) {
        if self.loaded {
            return;
        }
        let mut p2 = header.to_vec();
        write_slot(&mut p2, self.faces.id_slot);
        for (suffix, name) in scan_suffix_kv(store, &p2) {
            let Some(id) = I::from_be(&suffix) else {
                continue;
            };
            if let Ok(label) = String::from_utf8(name) {
                self.by_id.insert(id, label);
            }
        }
        // The label direction mirrors; also derives next_id (max + 1).
        let mut p3 = header.to_vec();
        write_slot(&mut p3, self.faces.name_slot);
        for (suffix, idv) in scan_suffix_kv(store, &p3) {
            if let (Ok(label), Some(id)) = (String::from_utf8(suffix), I::from_be(&idv)) {
                self.by_name.insert(label, id);
                if let Some(next) = id.succ() {
                    if next > self.next_id {
                        self.next_id = next;
                    }
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
        let mut d = VocabCache::<u16>::new(FIELD_VOCAB);
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
        let mut fields = VocabCache::<u16>::new(FIELD_VOCAB);
        let mut kinds = VocabCache::<u16>::new(PREDICATE_VOCAB);
        // The same label allocates in both; the same number names two
        // different things — that is precisely what the split permits.
        assert_eq!(fields.id_for(&mut store, HEADER, "since"), 0);
        assert_eq!(kinds.id_for(&mut store, HEADER, "has"), 0);
        assert_eq!(fields.name_for(&store, HEADER, 0).unwrap(), "since");
        assert_eq!(kinds.name_for(&store, HEADER, 0).unwrap(), "has");
        // …and they land on different faces.
        assert_eq!(
            store.get(&key(HEADER, PREDICATE_VOCAB_ID_SLOT, &0u16.to_be_bytes())).unwrap(),
            b"has"
        );
        assert_eq!(
            store.get(&key(HEADER, FIELD_VOCAB_ID_SLOT, &0u16.to_be_bytes())).unwrap(),
            b"since"
        );
    }

    #[test]
    fn fresh_cache_loads_from_engine_and_resumes_numbering() {
        let mut store = TestStore::slatedb_mem();
        {
            let mut d = VocabCache::<u16>::new(PREDICATE_VOCAB);
            d.id_for(&mut store, HEADER, "one");
            d.id_for(&mut store, HEADER, "two");
        }
        // A brand-new cache (process restart) resumes after the max.
        let mut d2 = VocabCache::<u16>::new(PREDICATE_VOCAB);
        assert_eq!(d2.name_for(&store, HEADER, 0).unwrap(), "one");
        assert_eq!(d2.name_for(&store, HEADER, 1).unwrap(), "two");
        let fresh = d2.id_for(&mut store, HEADER, "three");
        assert_eq!(fresh, 2, "numbering resumes after the loaded max");
        // A read-only handle answers without the caller priming anything.
        let mut d3 = VocabCache::<u16>::new(PREDICATE_VOCAB);
        assert_eq!(d3.id_get(&store, HEADER, "two"), Some(1));
    }

    #[test]
    fn a_sparse_seeded_id_still_reads_back() {
        let mut store = TestStore::slatedb_mem();
        let mut d = VocabCache::<u16>::new(FIELD_VOCAB);
        // Seed a high id directly, then read it through the cache.
        let mut batch = MemBatch::default();
        batch.put(key(HEADER, FIELD_VOCAB_ID_SLOT, &0x1234u16.to_be_bytes()), b"big".to_vec());
        batch.put(key(HEADER, FIELD_VOCAB_NAME_SLOT, b"big"), 0x1234u16.to_be_bytes().to_vec());
        let _ = store.commit_batch(batch);
        assert_eq!(d.name_for(&store, HEADER, 0x1234).unwrap(), "big");
    }

    #[test]
    fn data_vocabularies_share_one_counter_across_two_families() {
        // ADR-0032: one counter per declared Vocab<N> field; the DIRECTION is
        // the family, so field #0 is (0x2/1, 0x1/1) and field #1 is (0x2/2,
        // 0x1/2) — never a parity convention inside one family.
        let (id0, label0) = (data_vocab(0).id_slot, data_vocab(0).name_slot);
        let (id1, label1) = (data_vocab(1).id_slot, data_vocab(1).name_slot);
        assert_eq!(id0 >> 12, segment::VOCAB_ID as u16, "id → label family");
        assert_eq!(label0 >> 12, segment::VOCAB_LABEL as u16, "label → id family");
        assert_eq!(id0 & 0xFFF, label0 & 0xFFF, "one counter per vocabulary");
        assert_eq!(id1 & 0xFFF, (id0 & 0xFFF) + 1, "declaration order");
        assert_eq!(label1 & 0xFFF, id1 & 0xFFF);

        // Two fields no longer share an id space: the same number in two
        // fields names two different labels.
        let mut store = TestStore::slatedb_mem();
        let mut status = VocabCache::<u8>::new(data_vocab(0));
        let mut color = VocabCache::<u8>::new(data_vocab(1));
        assert_eq!(status.id_for(&mut store, HEADER, "active"), 0);
        assert_eq!(color.id_for(&mut store, HEADER, "red"), 0);
        assert_eq!(status.name_for(&store, HEADER, 0).unwrap(), "active");
        assert_eq!(color.name_for(&store, HEADER, 0).unwrap(), "red");
    }

    #[test]
    #[should_panic(expected = "vocabulary exhausted")]
    fn a_u8_vocabulary_stops_after_256_labels() {
        let mut store = TestStore::slatedb_mem();
        let mut d = VocabCache::<u8>::new(data_vocab(0));
        // 0..=255 all fit; the 257th label is the loud error.
        for i in 0..=256u16 {
            d.id_for(&mut store, HEADER, &format!("label-{i}"));
        }
    }
}