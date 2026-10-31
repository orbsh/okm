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

use crate::engine::storage::{Batch, VirtualStorage, scan_suffix_kv};
use crate::model::index::{
    FIELD_VOCAB_ID_SLOT, FIELD_VOCAB_NAME_SLOT, PREDICATE_VOCAB_ID_SLOT, PREDICATE_VOCAB_NAME_SLOT,
    Slot, segment, slot, write_slot,
};
use crate::model::vocab::VocabId;
use std::collections::HashMap;

/// The pair of faces one vocabulary occupies. Numbers are structural
/// (ADR-0030): a vocabulary is a pair of counting-family faces, never a band
/// inside another family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VocabFaces {
    /// id → label. Also carries the allocation cursor as its FACE-LEVEL
    /// SINGLETON: the key `[ns][id_slot]` with NO suffix, holding the next id.
    /// A real entry of this face is always `[ns][id_slot][id bytes]`, so the two
    /// cannot collide — the singleton is impossible to confuse with data by
    /// LENGTH, not by a reserved value. The label face cannot host one (the
    /// empty label is legal), which is why the cursor lives here.
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
        let counter = id_slot & 0x0FFF;
        Self {
            id_slot,
            name_slot: slot(segment::VOCAB_LABEL, counter),
        }
    }
}

/// The key of a vocabulary's allocation cursor: the id face's bare prefix,
/// `[ns][id_slot]` with no id bytes (ADR-0030's face-level singleton, applied
/// here by ADR-0032).
///
/// ONE definition for every reader and writer of the entry — the typed cache,
/// the dynamic mirror, the plan surface a host replays. The key is DERIVED from
/// the id face's number rather than carrying one, which is why it is not a
/// field of the exported schema: a host that allocates ids itself (rather than
/// through `plan_put_fields`) builds the key with this function instead of
/// re-deriving the rule.
pub fn cursor_key(header: &[u8], faces: VocabFaces) -> Vec<u8> {
    let mut key = header.to_vec();
    write_slot(&mut key, faces.id_slot);
    key
}

/// Point lookup of a label from raw id bytes, for readers that keep no cache
/// (the dynamic path): one engine probe, and `None` when the vocabulary has no
/// such id — a reader never invents a label.
pub fn vocab_label<S: VirtualStorage + ?Sized>(
    store: &S,
    header: &[u8],
    faces: VocabFaces,
    id_be: &[u8],
) -> crate::error::Result<Option<String>> {
    let mut key = header.to_vec();
    write_slot(&mut key, faces.id_slot);
    key.extend_from_slice(id_be);
    let raw = store.get(&key)?;
    Ok(raw.and_then(|raw| String::from_utf8(raw).ok()))
}

/// Inverse of [`vocab_label`]: the id bytes naming `label`, ALLOCATING on first
/// sight. A write-path call — export never needs it (export reads labels), the
/// Parquet import does: a snapshot carries labels, and restoring one has to
/// re-derive ids in the destination's own numbering.
///
/// `width` is the field's declared id width in bytes. `None` when no vocabulary
/// width matches — a schema/bridge disagreement, not a data condition.
pub fn vocab_id<S: VirtualStorage + ?Sized>(
    store: &mut S,
    header: &[u8],
    faces: VocabFaces,
    width: usize,
    label: &str,
) -> crate::error::Result<Option<Vec<u8>>> {
    // Allocation can fail (a full width, a refused commit), so the answer is
    // `Result<Option<_>>`: `None` stays the schema/bridge disagreement.
    Ok(Some(match width {
        1 => VocabId::to_be(VocabCache::<u8>::new(faces).id_for(store, header, label)?),
        2 => VocabId::to_be(VocabCache::<u16>::new(faces).id_for(store, header, label)?),
        4 => VocabId::to_be(VocabCache::<u32>::new(faces).id_for(store, header, label)?),
        8 => VocabId::to_be(VocabCache::<u64>::new(faces).id_for(store, header, label)?),
        16 => VocabId::to_be(VocabCache::<u128>::new(faces).id_for(store, header, label)?),
        _ => return Ok(None),
    }))
}

/// Field names of the payload, in the document's own ns (ADR-0012).
pub const FIELD_VOCAB: VocabFaces = VocabFaces {
    id_slot: FIELD_VOCAB_ID_SLOT,
    name_slot: FIELD_VOCAB_NAME_SLOT,
};
/// The collection's predicate vocabulary: a graph's predicates, a node
/// collection's kind tokens (ADR-0017 §3).
pub const PREDICATE_VOCAB: VocabFaces = VocabFaces {
    id_slot: PREDICATE_VOCAB_ID_SLOT,
    name_slot: PREDICATE_VOCAB_NAME_SLOT,
};

/// One vocabulary cache, bound to one ns segment, one face triple, and one id
/// width `I`.
///
/// The hot path never scans. A write probes the label face (a pending batch
/// counts — `BatchStore::get` answers from its ops), reads the cursor, and
/// commits three point writes; a read probes the id face. The in-memory maps are
/// a per-call memo, NOT a loaded snapshot: loading the vocabulary on first touch
/// cost a full two-face scan per `put`, because the derive builds one cache per
/// field per call.
#[derive(Debug)]
pub struct VocabCache<I: VocabId = u16> {
    faces: VocabFaces,
    by_id: HashMap<I, String>,
    by_name: HashMap<String, I>,
    /// The next id this cache knows about: `None` until the first allocation in
    /// this cache asks the store.
    next_id: Option<I>,
}

impl<I: VocabId> VocabCache<I> {
    pub fn new(faces: VocabFaces) -> Self {
        Self {
            faces,
            by_id: HashMap::new(),
            by_name: HashMap::new(),
            next_id: None,
        }
    }

    fn name_key(&self, header: &[u8], label: &str) -> Vec<u8> {
        let mut key = header.to_vec();
        write_slot(&mut key, self.faces.name_slot);
        key.extend_from_slice(label.as_bytes());
        key
    }

    fn id_key(&self, header: &[u8], id: I) -> Vec<u8> {
        let mut key = header.to_vec();
        write_slot(&mut key, self.faces.id_slot);
        key.extend_from_slice(&id.to_be());
        key
    }

    /// The face-level singleton: the id face's prefix with NO id suffix. Every
    /// real entry of that face carries exactly `I::WIDTH` bytes after the slot,
    /// so a zero-length key is not expressible as data. Delegates to
    /// [`cursor_key`] — the same definition the dynamic mirror and any host
    /// writer use.
    fn cursor_key(&self, header: &[u8]) -> Vec<u8> {
        cursor_key(header, self.faces)
    }

    /// Look up (or allocate) the id for `label`. `header` = the collection's key
    /// header (`[ns]`) — vocabulary keys live in the same ns segment as the data
    /// they name. Exhausting the width is an error, never a wrap: a wrapped id
    /// names another label forever.
    pub fn id_for<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &mut S,
        header: &[u8],
        label: &str,
    ) -> crate::error::Result<I> {
        // Already interned? ONE point probe — no scan, and through a BatchStore
        // it sees a label this same batch allocated earlier.
        let name_key = self.name_key(header, label);
        if let Some(id) = store.get(&name_key)?.and_then(|raw| I::from_be(&raw)) {
            self.remember(id, label);
            return Ok(id);
        }

        let id = self.take_next(store, header)?;
        let next = match id.succ() {
            Some(next) => next,
            None => {
                return Err(crate::error::Error::VocabularyExhausted {
                    width: I::WIDTH,
                    next: id.as_u128() + 1,
                });
            }
        };

        // ONE batch, three point writes: label → id, id → label, and the bumped
        // cursor. Through `BatchStore` the batch is the CALLER's batch, so a
        // batch writer gets vocabulary growth, its row and its faces atomically.
        let mut batch = Batch::default();
        let id_key = self.id_key(header, id);
        batch.put(name_key, id.to_be());
        batch.put(id_key, label.as_bytes().to_vec());
        batch.put(self.cursor_key(header), next.to_be());
        store.commit_batch(batch)?;

        self.next_id = Some(next);
        self.remember(id, label);
        Ok(id)
    }

    /// Resolve an id to its label. One point probe (memoized); an id the
    /// vocabulary does not know yields `None` — the caller leaves the value
    /// unresolved rather than inventing a name.
    pub fn name_for<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &S,
        header: &[u8],
        id: I,
    ) -> crate::error::Result<Option<String>> {
        if let Some(name) = self.by_id.get(&id) {
            return Ok(Some(name.clone()));
        }
        let raw = match store.get(&self.id_key(header, id))? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        // Bytes that are not a label: the entry is corrupt, and an unknown id
        // is the answer the caller already handles (never an invented name).
        let Ok(name) = String::from_utf8(raw) else {
            return Ok(None);
        };
        self.remember(id, &name);
        Ok(Some(name))
    }

    /// Non-allocating label → id probe. One point probe (memoized).
    pub fn id_get<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &S,
        header: &[u8],
        label: &str,
    ) -> crate::error::Result<Option<I>> {
        if let Some(id) = self.by_name.get(label) {
            return Ok(Some(*id));
        }
        let raw = match store.get(&self.name_key(header, label))? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        let Some(id) = I::from_be(&raw) else {
            return Ok(None);
        };
        self.remember(id, label);
        Ok(Some(id))
    }

    fn remember(&mut self, id: I, label: &str) {
        self.by_id.insert(id, label.to_owned());
        self.by_name.insert(label.to_owned(), id);
    }

    /// The next id to hand out: the cursor entry when the store has one, else —
    /// a vocabulary that has never allocated — id zero. A store written before
    /// cursors existed is recognisable WITHOUT a scan: id zero is absent exactly
    /// when the vocabulary is empty (ids are never deleted), so a populated
    /// legacy vocabulary falls through to the one-time max + 1 scan, and the
    /// allocation writes the cursor that retires that path forever.
    fn take_next<S: VirtualStorage + ?Sized>(
        &mut self,
        store: &S,
        header: &[u8],
    ) -> crate::error::Result<I> {
        if let Some(next) = self.next_id {
            return Ok(next);
        }
        let next = match store
            .get(&self.cursor_key(header))?
            .and_then(|raw| I::from_be(&raw))
        {
            Some(cursor) => cursor,
            None => {
                if store.get(&self.id_key(header, I::ZERO))?.is_none() {
                    I::ZERO
                } else {
                    self.scan_max_plus_one(store, header)?
                }
            }
        };
        self.next_id = Some(next);
        Ok(next)
    }

    fn scan_max_plus_one<S: VirtualStorage + ?Sized>(
        &self,
        store: &S,
        header: &[u8],
    ) -> crate::error::Result<I> {
        let mut prefix = header.to_vec();
        write_slot(&mut prefix, self.faces.id_slot);
        let mut next = I::ZERO;
        for (suffix, _) in scan_suffix_kv(store, &prefix)? {
            if let Some(id) = I::from_be(&suffix)
                && let Some(after) = id.succ()
            {
                next = next.max(after);
            }
        }
        Ok(next)
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
        let a = d.id_for(&mut store, HEADER, "alpha").unwrap();
        let b = d.id_for(&mut store, HEADER, "beta").unwrap();
        let a2 = d.id_for(&mut store, HEADER, "alpha").unwrap();
        assert_eq!(
            (a, b, a2),
            (0, 1, 0),
            "first-seen claims next, repeat hits cache"
        );

        // Both directions landed in the engine.
        assert_eq!(
            store
                .get(&key(HEADER, FIELD_VOCAB_ID_SLOT, &0u16.to_be_bytes()))
                .unwrap()
                .unwrap(),
            b"alpha".to_vec()
        );
        assert_eq!(
            store
                .get(&key(HEADER, FIELD_VOCAB_NAME_SLOT, b"beta"))
                .unwrap()
                .unwrap(),
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
        assert_eq!(fields.id_for(&mut store, HEADER, "since").unwrap(), 0);
        assert_eq!(kinds.id_for(&mut store, HEADER, "has").unwrap(), 0);
        assert_eq!(
            fields.name_for(&store, HEADER, 0).unwrap().unwrap(),
            "since"
        );
        assert_eq!(kinds.name_for(&store, HEADER, 0).unwrap().unwrap(), "has");
        // …and they land on different faces.
        assert_eq!(
            store
                .get(&key(HEADER, PREDICATE_VOCAB_ID_SLOT, &0u16.to_be_bytes()))
                .unwrap()
                .unwrap(),
            b"has".to_vec()
        );
        assert_eq!(
            store
                .get(&key(HEADER, FIELD_VOCAB_ID_SLOT, &0u16.to_be_bytes()))
                .unwrap()
                .unwrap(),
            b"since".to_vec()
        );
    }

    #[test]
    fn fresh_cache_loads_from_engine_and_resumes_numbering() {
        let mut store = TestStore::slatedb_mem();
        {
            let mut d = VocabCache::<u16>::new(PREDICATE_VOCAB);
            d.id_for(&mut store, HEADER, "one").unwrap();
            d.id_for(&mut store, HEADER, "two").unwrap();
        }
        // A brand-new cache (process restart) resumes after the max.
        let mut d2 = VocabCache::<u16>::new(PREDICATE_VOCAB);
        assert_eq!(d2.name_for(&store, HEADER, 0).unwrap().unwrap(), "one");
        assert_eq!(d2.name_for(&store, HEADER, 1).unwrap().unwrap(), "two");
        let fresh = d2.id_for(&mut store, HEADER, "three").unwrap();
        assert_eq!(fresh, 2, "numbering resumes after the loaded max");
        // A read-only handle answers without the caller priming anything.
        let mut d3 = VocabCache::<u16>::new(PREDICATE_VOCAB);
        assert_eq!(d3.id_get(&store, HEADER, "two").unwrap(), Some(1));
    }

    #[test]
    fn a_sparse_seeded_id_still_reads_back() {
        let store = TestStore::slatedb_mem();
        let mut d = VocabCache::<u16>::new(FIELD_VOCAB);
        // Seed a high id directly, then read it through the cache.
        let mut batch = Batch::default();
        batch.put(
            key(HEADER, FIELD_VOCAB_ID_SLOT, &0x1234u16.to_be_bytes()),
            b"big".to_vec(),
        );
        batch.put(
            key(HEADER, FIELD_VOCAB_NAME_SLOT, b"big"),
            0x1234u16.to_be_bytes().to_vec(),
        );
        let _ = store.commit_batch(batch);
        assert_eq!(d.name_for(&store, HEADER, 0x1234).unwrap().unwrap(), "big");
    }

    #[test]
    fn data_vocabularies_share_one_counter_across_two_families() {
        // ADR-0032: one counter per declared Vocab<N> field; the DIRECTION is
        // the family, so field #0 is (0x2/1, 0x1/1) and field #1 is (0x2/2,
        // 0x1/2) — never a parity convention inside one family.
        let (id0, label0) = (data_vocab(0).id_slot, data_vocab(0).name_slot);
        let (id1, label1) = (data_vocab(1).id_slot, data_vocab(1).name_slot);
        assert_eq!(id0 >> 12, segment::VOCAB_ID, "id → label family");
        assert_eq!(label0 >> 12, segment::VOCAB_LABEL, "label → id family");
        assert_eq!(id0 & 0xFFF, label0 & 0xFFF, "one counter per vocabulary");
        assert_eq!(id1 & 0xFFF, (id0 & 0xFFF) + 1, "declaration order");
        assert_eq!(label1 & 0xFFF, id1 & 0xFFF);

        // Two fields no longer share an id space: the same number in two
        // fields names two different labels.
        let mut store = TestStore::slatedb_mem();
        let mut status = VocabCache::<u8>::new(data_vocab(0));
        let mut color = VocabCache::<u8>::new(data_vocab(1));
        assert_eq!(status.id_for(&mut store, HEADER, "active").unwrap(), 0);
        assert_eq!(color.id_for(&mut store, HEADER, "red").unwrap(), 0);
        assert_eq!(
            status.name_for(&store, HEADER, 0).unwrap().unwrap(),
            "active"
        );
        assert_eq!(color.name_for(&store, HEADER, 0).unwrap().unwrap(), "red");
    }

    #[test]
    fn a_u8_vocabulary_stops_after_256_labels() {
        let mut store = TestStore::slatedb_mem();
        let mut d = VocabCache::<u8>::new(data_vocab(0));
        // Ids 0..=254 fit: allocation needs `succ()` to exist so the cursor can
        // be bumped, so the last usable id is one short of the width. The 256th
        // label is refused — an error, not a wrap and not a panic (ADR-0037: a
        // write verb reports, it does not abort the process).
        for i in 0..=254u16 {
            d.id_for(&mut store, HEADER, &format!("label-{i}"))
                .expect("ids 0..=254 all fit");
        }
        let err = d
            .id_for(&mut store, HEADER, "label-255")
            .expect_err("a u8 vocabulary is full");
        assert!(
            matches!(
                err,
                crate::error::Error::VocabularyExhausted { width: 1, .. }
            ),
            "unexpected error: {err:?}"
        );
    }
}
