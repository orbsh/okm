//! Name vocabularies (ADR-0012, ADR-0017 §3, ADR-0032, ADR-0060 §6): the
//! run-time, per-collection bidirectional mapping between a label and its
//! number. Each vocabulary is two point-lookup faces inside the collection's ns:
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
//! THE CACHE IS NOT HERE. `okm-value` owns it — both directions, the lazy
//! probe-before-allocate rule, and the pending window — because it is the same
//! cache for every vocabulary and because nothing about it is the store's
//! business (ADR-0060 §6). What stays in this module is what the store's key
//! layout makes this module's own: the faces, the keys built from them, the
//! allocation cursor, and [`Faces`] — the four-method door the cache calls
//! through. Swapping the cache's data structure changes no line here.
//!
//! Discipline copied from the ns vocabulary (ADR-0002): append-only — ids are
//! never reused, never renumbered; a label seen for the first time on the write
//! path claims the next free number (single-writer allocation, serialized by
//! the engine mutex — the same boundary a local caller's `&mut self` provides).
//! Exhausting the declared width is a loud error, never a wrap.

use crate::engine::storage::{Batch, VirtualStorage, scan_suffix_kv};
use crate::model::index::{
    FIELD_VOCAB_ID_SLOT, FIELD_VOCAB_NAME_SLOT, PREDICATE_VOCAB_ID_SLOT, PREDICATE_VOCAB_NAME_SLOT,
    Slot, segment, slot, write_slot,
};
pub use okm_value::{Id, Names, Vocabulary};

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

/// The key naming `label` in a vocabulary's label→id face.
pub fn name_key(header: &[u8], faces: VocabFaces, label: &str) -> Vec<u8> {
    let mut key = header.to_vec();
    write_slot(&mut key, faces.name_slot);
    key.extend_from_slice(label.as_bytes());
    key
}

/// The key naming `id` in a vocabulary's id→label face.
pub fn id_key(header: &[u8], faces: VocabFaces, id: impl Id) -> Vec<u8> {
    let mut key = header.to_vec();
    write_slot(&mut key, faces.id_slot);
    key.extend_from_slice(&id.be_bytes());
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
///
/// Takes `&S`, not `&mut S`: writes already take `&self` (ADR-0026), and a caller
/// holding a shared handle — a replayer beside its target collection, an importer
/// beside its store — is exactly who needs to intern a label.
pub fn vocab_id<S: VirtualStorage + ?Sized>(
    store: &S,
    header: &[u8],
    faces: VocabFaces,
    width: usize,
    label: &str,
) -> crate::error::Result<Option<Vec<u8>>> {
    // Allocation can fail (a full width, a refused commit), so the answer is
    // `Result<Option<_>>`: `None` stays the schema/bridge disagreement.
    Ok(Some(match width {
        1 => Vocabulary::<u8>::new()
            .ensure(&mut Faces::<_, u8>::new(store, header, faces), label)?
            .be_bytes(),
        2 => Vocabulary::<u16>::new()
            .ensure(&mut Faces::<_, u16>::new(store, header, faces), label)?
            .be_bytes(),
        4 => Vocabulary::<u32>::new()
            .ensure(&mut Faces::<_, u32>::new(store, header, faces), label)?
            .be_bytes(),
        8 => Vocabulary::<u64>::new()
            .ensure(&mut Faces::<_, u64>::new(store, header, faces), label)?
            .be_bytes(),
        16 => Vocabulary::<u128>::new()
            .ensure(&mut Faces::<_, u128>::new(store, header, faces), label)?
            .be_bytes(),
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

/// The store door for one vocabulary: the ns header, the two faces, and a
/// store to probe.
///
/// This is the only place that knows a vocabulary is made of keys. The cache in
/// `okm-value` calls it on a miss and never learns what a slot is; a different
/// key layout would mean a different door here and no change there.
pub struct Faces<'a, S: ?Sized, I: Id> {
    store: &'a S,
    header: &'a [u8],
    faces: VocabFaces,
    id: core::marker::PhantomData<I>,
}

impl<'a, S: VirtualStorage + ?Sized, I: Id> Faces<'a, S, I> {
    pub fn new(store: &'a S, header: &'a [u8], faces: VocabFaces) -> Self {
        Self {
            store,
            header,
            faces,
            id: core::marker::PhantomData,
        }
    }

    /// The next id to hand out: the cursor entry when the store has one, else —
    /// a vocabulary that has never allocated — id zero. A store written before
    /// cursors existed is recognisable WITHOUT a scan: id zero is absent exactly
    /// when the vocabulary is empty (ids are never deleted), so a populated
    /// legacy vocabulary falls through to the one-time max + 1 scan, and the
    /// allocation writes the cursor that retires that path forever.
    ///
    /// The cursor is read EVERY time, never cached: a second handle on the same
    /// table allocates from the same counter, and a cached watermark would hand
    /// out an id the other handle just took. One probe per NEW label is the
    /// whole cost — labels are allocated once, ever.
    fn take_next(&self) -> crate::error::Result<I> {
        let next = match self
            .store
            .get(&cursor_key(self.header, self.faces))?
            .and_then(|raw| I::from_be(&raw))
        {
            Some(cursor) => cursor,
            None => {
                if self.store.get(&id_key(self.header, self.faces, I::ZERO))?.is_none() {
                    I::ZERO
                } else {
                    self.scan_max_plus_one()?
                }
            }
        };
        Ok(next)
    }

    fn scan_max_plus_one(&self) -> crate::error::Result<I> {
        let mut prefix = self.header.to_vec();
        write_slot(&mut prefix, self.faces.id_slot);
        let mut next = I::ZERO;
        for (suffix, _) in scan_suffix_kv(self.store, &prefix)? {
            if let Some(id) = I::from_be(&suffix)
                && let Some(after) = id.succ()
            {
                next = next.max(after);
            }
        }
        Ok(next)
    }
}

impl<S: VirtualStorage + ?Sized, I: Id> Names<I> for Faces<'_, S, I> {
    type Error = crate::error::Error;

    fn scan(&mut self) -> crate::error::Result<Vec<(String, I)>> {
        let mut prefix = self.header.to_vec();
        write_slot(&mut prefix, self.faces.id_slot);
        let mut pairs = Vec::new();
        for (suffix, label) in scan_suffix_kv(self.store, &prefix)? {
            let (Some(id), Ok(label)) = (I::from_be(&suffix), String::from_utf8(label)) else {
                // A torn or foreign entry: it names nothing a reader could use,
                // so it contributes no pair rather than a wrong one.
                continue;
            };
            pairs.push((label, id));
        }
        Ok(pairs)
    }

    fn name_id(&mut self, label: &str) -> crate::error::Result<Option<I>> {
        // Through a `BatchStore` this sees a label the same batch allocated
        // earlier, which is why a pending label's probe is not wasted.
        Ok(self
            .store
            .get(&name_key(self.header, self.faces, label))?
            .and_then(|raw| I::from_be(&raw)))
    }

    fn id_name(&mut self, id: I) -> crate::error::Result<Option<String>> {
        let raw = match self.store.get(&id_key(self.header, self.faces, id))? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        // Bytes that are not a label: the entry is corrupt, and an unknown id
        // is the answer the caller already handles (never an invented name).
        Ok(String::from_utf8(raw).ok())
    }

    fn allocate(&mut self, label: &str) -> crate::error::Result<I> {
        let id = self.take_next()?;
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
        batch.put(name_key(self.header, self.faces, label), id.be_bytes());
        batch.put(id_key(self.header, self.faces, id), label.as_bytes().to_vec());
        batch.put(cursor_key(self.header, self.faces), next.be_bytes());
        self.store.commit_batch(batch)?;
        Ok(id)
    }
}

/// The pending ids whose label key IS in `ops` — the promotion candidates for a
/// commit that is about to land. Call it before the commit (the batch is
/// consumed by it) and [`Vocabulary::landed`] with the answer only after it
/// returned `Ok`: a refused commit promotes nothing, and its labels keep probing
/// (correct, one probe slower).
///
/// The set of pending labels is the cache's; which key a label lands on is this
/// module's. That is why the matching happens here and not in `okm-value`.
pub fn pending_in<I: Id>(
    vocabulary: &Vocabulary<I>,
    header: &[u8],
    faces: VocabFaces,
    ops: &[(Vec<u8>, Option<Vec<u8>>)],
) -> Vec<I> {
    if vocabulary.pending_len() == 0 {
        return Vec::new();
    }
    vocabulary
        .pending()
        .filter(|(label, _)| {
            let key = name_key(header, faces, label);
            ops.iter()
                .any(|(written, value)| value.is_some() && written == &key)
        })
        .map(|(_, id)| id)
        .collect()
}

// The engine-backed half of this module's tests needs a real store, and `test_engine` only
// exists when a backend feature is on — so the module rides the same gate instead of failing
// the whole crate's unit-test build with no backend compiled in.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mem_store::MemStore;

    const HEADER: &[u8] = &[0x00, 0x21]; // ns 33, no partition

    fn key(header: &[u8], slot: Slot, suffix: &[u8]) -> Vec<u8> {
        let mut k = header.to_vec();
        write_slot(&mut k, slot);
        k.extend_from_slice(suffix);
        k
    }

    #[test]
    fn allocates_sequentially_and_is_cached() {
        let store = MemStore::new();
        let mut door = Faces::<_, u16>::new(&store, HEADER, FIELD_VOCAB);
        let mut d = Vocabulary::<u16>::new();
        let a = d.ensure(&mut door, "alpha").unwrap();
        let b = d.ensure(&mut door, "beta").unwrap();
        let a2 = d.ensure(&mut door, "alpha").unwrap();
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
        let store = MemStore::new();
        let mut fields = Vocabulary::<u16>::new();
        let mut kinds = Vocabulary::<u16>::new();
        // The same label allocates in both; the same number names two
        // different things — that is precisely what the split permits.
        let mut fd = Faces::<_, u16>::new(&store, HEADER, FIELD_VOCAB);
        let mut kd = Faces::<_, u16>::new(&store, HEADER, PREDICATE_VOCAB);
        assert_eq!(fields.ensure(&mut fd, "since").unwrap(), 0);
        assert_eq!(kinds.ensure(&mut kd, "has").unwrap(), 0);
        assert_eq!(fields.name_of(&mut fd, 0).unwrap().unwrap(), "since");
        assert_eq!(kinds.name_of(&mut kd, 0).unwrap().unwrap(), "has");
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
        let store = MemStore::new();
        {
            let mut d = Vocabulary::<u16>::new();
            let mut door = Faces::<_, u16>::new(&store, HEADER, PREDICATE_VOCAB);
            d.ensure(&mut door, "one").unwrap();
            d.ensure(&mut door, "two").unwrap();
        }
        // A brand-new cache (process restart) resumes after the max.
        let mut d2 = Vocabulary::<u16>::new();
        let mut door = Faces::<_, u16>::new(&store, HEADER, PREDICATE_VOCAB);
        assert_eq!(d2.name_of(&mut door, 0).unwrap().unwrap(), "one");
        assert_eq!(d2.name_of(&mut door, 1).unwrap().unwrap(), "two");
        let fresh = d2.ensure(&mut door, "three").unwrap();
        assert_eq!(fresh, 2, "numbering resumes after the loaded max");
        // A read-only handle answers without the caller priming anything.
        let mut d3 = Vocabulary::<u16>::new();
        assert_eq!(d3.id_of(&mut door, "two").unwrap(), Some(1));
    }

    #[test]
    fn a_sparse_seeded_id_still_reads_back() {
        let store = MemStore::new();
        let mut d = Vocabulary::<u16>::new();
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
        let mut door = Faces::<_, u16>::new(&store, HEADER, FIELD_VOCAB);
        assert_eq!(d.name_of(&mut door, 0x1234).unwrap().unwrap(), "big");
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
        let store = MemStore::new();
        let mut status = Vocabulary::<u8>::new();
        let mut color = Vocabulary::<u8>::new();
        let mut sd = Faces::<_, u8>::new(&store, HEADER, data_vocab(0));
        let mut cd = Faces::<_, u8>::new(&store, HEADER, data_vocab(1));
        assert_eq!(status.ensure(&mut sd, "active").unwrap(), 0);
        assert_eq!(color.ensure(&mut cd, "red").unwrap(), 0);
        assert_eq!(status.name_of(&mut sd, 0).unwrap().unwrap(), "active");
        assert_eq!(color.name_of(&mut cd, 0).unwrap().unwrap(), "red");
    }

    #[test]
    fn a_u8_vocabulary_stops_after_256_labels() {
        let store = MemStore::new();
        let mut d = Vocabulary::<u8>::new();
        let mut door = Faces::<_, u8>::new(&store, HEADER, data_vocab(0));
        // Ids 0..=254 fit: allocation needs `succ()` to exist so the cursor can
        // be bumped, so the last usable id is one short of the width. The 256th
        // label is refused — an error, not a wrap and not a panic (ADR-0037: a
        // write verb reports, it does not abort the process).
        for i in 0..=254u16 {
            d.ensure(&mut door, &format!("label-{i}"))
                .expect("ids 0..=254 all fit");
        }
        let err = d
            .ensure(&mut door, "label-255")
            .expect_err("a u8 vocabulary is full");
        assert!(
            matches!(
                err,
                crate::error::Error::VocabularyExhausted { width: 1, .. }
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn an_allocated_id_is_only_evidence_after_the_commit_carrying_it() {
        let store = MemStore::new();
        let mut d = Vocabulary::<u16>::new();
        let mut door = Faces::<_, u16>::new(&store, HEADER, FIELD_VOCAB);
        assert_eq!(d.ensure(&mut door, "alpha").unwrap(), 0);
        assert_eq!(d.pending_len(), 1, "an allocation is not evidence yet");

        // A commit that does NOT carry the name key promotes nothing — which is
        // exactly what a caller's batch that never landed looks like.
        let mut other = Batch::default();
        other.put(b"elsewhere".to_vec(), vec![1]);
        assert!(pending_in(&d, HEADER, FIELD_VOCAB, &other.ops).is_empty());
        assert_eq!(d.pending_len(), 1);

        // The commit that carries it does.
        let mut commit = Batch::default();
        commit.put(
            key(HEADER, FIELD_VOCAB_NAME_SLOT, b"alpha"),
            0u16.to_be_bytes().to_vec(),
        );
        let promoted = pending_in(&d, HEADER, FIELD_VOCAB, &commit.ops);
        assert_eq!(promoted, vec![0]);
        d.landed(&promoted);
        assert_eq!(d.pending_len(), 0);
    }

    #[test]
    fn a_pending_id_is_probed_and_a_promoted_one_is_not() {
        // The observable difference between "the mirror says so" and "the store
        // says so": delete the engine's entry behind the cache's back. A probe
        // re-allocates; the fast path answers the mirror.
        let store = MemStore::new();
        let mut d = Vocabulary::<u16>::new();
        let mut door = Faces::<_, u16>::new(&store, HEADER, FIELD_VOCAB);
        assert_eq!(d.ensure(&mut door, "alpha").unwrap(), 0);
        store
            .del(&key(HEADER, FIELD_VOCAB_NAME_SLOT, b"alpha"))
            .unwrap();
        // Still pending: the cache asks the store, the store has nothing, so a
        // NEW id comes back — the first answer was never trusted.
        assert_eq!(d.ensure(&mut door, "alpha").unwrap(), 1);

        // Promoted: no probe, so the id survives the removal (a probe would
        // have handed out the next free id instead).
        let id = d.ensure(&mut door, "gamma").unwrap();
        let mut commit = Batch::default();
        commit.put(
            key(HEADER, FIELD_VOCAB_NAME_SLOT, b"gamma"),
            id.to_be_bytes().to_vec(),
        );
        let promoted = pending_in(&d, HEADER, FIELD_VOCAB, &commit.ops);
        d.landed(&promoted);
        store
            .del(&key(HEADER, FIELD_VOCAB_NAME_SLOT, b"gamma"))
            .unwrap();
        assert_eq!(d.ensure(&mut door, "gamma").unwrap(), id);
    }
}