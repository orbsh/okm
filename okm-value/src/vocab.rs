//! Name vocabularies: the label ↔ number mapping a dynamic segment is written in.
//!
//! A segment carries ids, not names, so both directions of the mapping cross the
//! codec's boundary: writing needs `label -> id`, reading needs `id -> label`.
//! This crate owns the cache over both — its shape, its lifetime rules, and the
//! laziness that keeps an untouched vocabulary free. It owns no storage: the
//! four methods of [`Names`] are the whole of what it needs, and the caller
//! implements them where its key layout lives.
//!
//! ```text
//! ensure(door, "addr")   hit in memory       -> the id, no probe
//!                        miss               -> door.name_id -> adopt, or
//!                                              door.allocate -> new id, pending
//! name_of(door, 7)       hit in memory       -> the label, no probe
//!                        miss               -> door.id_name -> adopt, or None
//! ```
//!
//! Two rules are the cache's, not the caller's:
//!
//! - **A miss probes before it allocates.** Two handles on one store share the
//!   allocation cursor but not the cache, so a label this cache has never seen
//!   may already have an id; minting a second one would leave one name with two
//!   ids forever.
//! - **An allocated id is not evidence until its write lands.** Allocation goes
//!   into the caller's transaction, and a refused commit leaves no vocabulary
//!   entry behind — so the id stays [`pending`](Vocabulary::pending_len) and is
//!   re-probed until the caller reports the commit ([`landed`](Vocabulary::landed)).

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};

/// A vocabulary id: one unsigned width, big-endian in the store.
pub trait Id: Copy + Eq + Hash + Ord + std::fmt::Debug + 'static {
    /// Bytes on the wire, and the width the caller's allocator refuses to exceed.
    const WIDTH: usize;
    /// The id a vocabulary hands out first. Zero means "empty" in the store,
    /// where the id face's own prefix is the sentinel.
    const ZERO: Self;

    fn succ(self) -> Option<Self>;
    /// Big-endian wire form, `WIDTH` bytes. Named `be_bytes` because every
    /// integer already has an inherent `to_be` that returns itself.
    fn be_bytes(self) -> Vec<u8>;
    /// The big-endian image right-aligned in sixteen bytes, so the last
    /// `WIDTH` bytes are the wire form without an allocation.
    fn be16(self) -> [u8; 16];
    fn from_be(bytes: &[u8]) -> Option<Self>;
    /// For messages and tests.
    fn as_u128(self) -> u128;

    /// The dense index, for the widths where an id is a small array position.
    fn as_index(self) -> Option<usize>;
    fn from_index(index: usize) -> Option<Self>;
}

macro_rules! id_width {
    ($ty:ty, $width:expr) => {
        impl Id for $ty {
            const WIDTH: usize = $width;
            const ZERO: Self = 0;

            fn succ(self) -> Option<Self> {
                self.checked_add(1)
            }
            fn be_bytes(self) -> Vec<u8> {
                self.to_be_bytes().to_vec()
            }
            fn be16(self) -> [u8; 16] {
                let mut out = [0u8; 16];
                out[16 - $width..].copy_from_slice(&self.to_be_bytes());
                out
            }
            fn from_be(bytes: &[u8]) -> Option<Self> {
                let bytes: [u8; $width] = bytes.try_into().ok()?;
                Some(<$ty>::from_be_bytes(bytes))
            }
            fn as_u128(self) -> u128 {
                self as u128
            }
            fn as_index(self) -> Option<usize> {
                usize::try_from(self).ok()
            }
            fn from_index(index: usize) -> Option<Self> {
                <$ty>::try_from(index).ok()
            }
        }
    };
}

id_width!(u8, 1);
id_width!(u16, 2);
id_width!(u32, 4);
id_width!(u64, 8);

impl Id for u128 {
    const WIDTH: usize = 16;
    const ZERO: Self = 0;

    fn succ(self) -> Option<Self> {
        self.checked_add(1)
    }
    fn be_bytes(self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
    fn be16(self) -> [u8; 16] {
        self.to_be_bytes()
    }
    fn from_be(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; 16] = bytes.try_into().ok()?;
        Some(u128::from_be_bytes(bytes))
    }
    fn as_u128(self) -> u128 {
        self
    }
    fn as_index(self) -> Option<usize> {
        None
    }
    fn from_index(_index: usize) -> Option<Self> {
        None
    }
}

/// The store door: everything the cache needs from the side that has the keys.
///
/// Four methods, all data in and data out — no namespace, no slot, no batch
/// appears here, so this crate never learns the caller's key layout. `Error` is
/// the caller's own error type, so a failure crosses the seam unchanged.
pub trait Names<I: Id> {
    type Error;

    /// Everything the store already knows. The warm path: one scan, and the
    /// caller decides when a scan is worth it.
    fn scan(&mut self) -> Result<Vec<(String, I)>, Self::Error>;

    /// The label's id, if the store has one. No allocation.
    fn name_id(&mut self, name: &str) -> Result<Option<I>, Self::Error>;

    /// The id's label, if the store has one.
    fn id_name(&mut self, id: I) -> Result<Option<String>, Self::Error>;

    /// Mint the next id for this label and make it durable alongside whatever
    /// write is in flight; the caller owns the cursor and the transaction.
    fn allocate(&mut self, name: &str) -> Result<I, Self::Error>;
}

/// One vocabulary's cache, both directions.
///
/// `id -> label` is an array while the id width makes an index cheap (u8, u16)
/// and a map above that; the array direction is the one every decode walks, and
/// it never hashes. `label -> id` is one map with a keyed fast hasher over the
/// same single copy of each label.
#[derive(Debug)]
pub struct Vocabulary<I: Id = u16> {
    by_name: HashMap<Box<str>, I, FastBuild>,
    by_id: ById<I>,
    /// Labels whose `allocate` this cache has not watched land. Lookups probe
    /// for these instead of trusting the mirror.
    pending: HashMap<Box<str>, I>,
}

impl<I: Id> Default for Vocabulary<I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Id> Vocabulary<I> {
    pub fn new() -> Self {
        Self {
            by_name: HashMap::default(),
            by_id: ById::new(),
            pending: HashMap::new(),
        }
    }

    /// Room for `n` labels, when the caller knows how many there are (a scan's
    /// length, a declaration's field count).
    pub fn with_capacity(n: usize) -> Self {
        let mut vocab = Self::new();
        vocab.by_name.reserve(n);
        vocab.by_id.reserve(n);
        vocab
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many allocations this cache has not watched land.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// The pending labels and their ids. The caller owns the key layout, so the
    /// matching against a batch is the caller's job; this is the data it needs.
    pub fn pending(&self) -> impl Iterator<Item = (&str, I)> + '_ {
        self.pending.iter().map(|(name, id)| (&**name, *id))
    }

    /// Take in what a scan found. Merges rather than replaces: another handle
    /// may have written between the scan and here, and a reload must not undo
    /// what this cache already learned.
    pub fn init<D: Names<I>>(&mut self, door: &mut D) -> Result<(), D::Error> {
        let pairs = door.scan()?;
        self.by_name.reserve(pairs.len());
        self.by_id.reserve(pairs.len());
        for (name, id) in pairs {
            self.remember(id, &name);
        }
        Ok(())
    }

    /// The id for `label`, allocating only if the store has never seen it.
    ///
    /// A label in `pending` falls through to the probe on purpose: its write may
    /// have been refused, so the mirror is not evidence yet.
    pub fn ensure<D: Names<I>>(&mut self, door: &mut D, label: &str) -> Result<I, D::Error> {
        if !self.pending.contains_key(label)
            && let Some(id) = self.by_name.get(label)
        {
            return Ok(*id);
        }
        // Already interned by someone else? Adopt, never allocate twice.
        if let Some(id) = door.name_id(label)? {
            self.remember(id, label);
            return Ok(id);
        }
        let id = door.allocate(label)?;
        self.remember(id, label);
        // Not evidence yet: the write is in the caller's transaction, and the
        // commit that carries it promotes it ([`Self::landed`]).
        self.pending.insert(label.into(), id);
        Ok(id)
    }

    /// The label for `id`. `None` is an answer — this vocabulary has no such id,
    /// so the caller leaves the value unresolved rather than inventing a name.
    pub fn name_of<D: Names<I>>(
        &mut self,
        door: &mut D,
        id: I,
    ) -> Result<Option<String>, D::Error> {
        if let Some(name) = self.by_id.get(id) {
            return Ok(Some(name.to_owned()));
        }
        let Some(name) = door.id_name(id)? else {
            return Ok(None);
        };
        self.remember(id, &name);
        Ok(Some(name))
    }

    /// The id for `label`, without allocating. `None` means the label is not in
    /// this vocabulary — a predicate nobody wrote, a field nobody declared.
    pub fn id_of<D: Names<I>>(&mut self, door: &mut D, label: &str) -> Result<Option<I>, D::Error> {
        if let Some(id) = self.by_name.get(label) {
            return Ok(Some(*id));
        }
        let Some(id) = door.name_id(label)? else {
            return Ok(None);
        };
        self.remember(id, label);
        Ok(Some(id))
    }

    /// These ids are durable now: the commit that carried their vocabulary
    /// entries returned `Ok`. Call it only after that, and only with the ids
    /// that write actually carried — a refused commit promotes nothing, and its
    /// labels keep probing (correct, one probe slower).
    pub fn landed(&mut self, ids: &[I]) {
        if ids.is_empty() || self.pending.is_empty() {
            return;
        }
        self.pending.retain(|_, id| !ids.contains(id));
    }

    /// Trust every pending id at once. Valid only where each allocation commits
    /// on its own (no batch staging): there a successful `ensure` has already
    /// made the entry durable.
    pub fn landed_all(&mut self) {
        self.pending.clear();
    }

    fn remember(&mut self, id: I, label: &str) {
        self.by_id.insert(id, label);
        if let Some(old) = self.by_name.insert(label.into(), id) {
            // A label that already had another id: the append-only rule says
            // this cannot happen, so keep both directions consistent with the
            // newest answer and drop nothing else.
            let _ = old;
        }
    }
}

/// The `id -> label` side: an array for the narrow widths, a map above them.
#[derive(Debug)]
enum ById<I: Id> {
    Dense(Vec<Option<Box<str>>>),
    Sparse(HashMap<I, Box<str>>),
}

impl<I: Id> ById<I> {
    /// An id is an array position only while the width keeps the array small:
    /// u8 and u16 vocabularies are the ones a table actually uses, and a u64 or
    /// u128 id space cannot be indexed by its own value.
    const DENSE: bool = I::WIDTH <= 2;

    fn new() -> Self {
        if Self::DENSE {
            ById::Dense(Vec::new())
        } else {
            ById::Sparse(HashMap::new())
        }
    }

    fn len(&self) -> usize {
        match self {
            ById::Dense(slots) => slots.iter().filter(|slot| slot.is_some()).count(),
            ById::Sparse(map) => map.len(),
        }
    }

    fn reserve(&mut self, n: usize) {
        match self {
            ById::Dense(slots) => slots.reserve(n),
            ById::Sparse(map) => map.reserve(n),
        }
    }

    fn get(&self, id: I) -> Option<&str> {
        match self {
            ById::Dense(slots) => {
                let index = id.as_index()?;
                slots.get(index).and_then(|slot| slot.as_deref())
            }
            ById::Sparse(map) => map.get(&id).map(|name| &**name),
        }
    }

    fn insert(&mut self, id: I, label: &str) {
        match self {
            ById::Dense(slots) => {
                let Some(index) = id.as_index() else {
                    return;
                };
                if slots.len() <= index {
                    slots.resize_with(index + 1, || None);
                }
                slots[index] = Some(label.into());
            }
            ById::Sparse(map) => {
                map.insert(id, label.into());
            }
        }
    }
}

/// A keyed fast hasher for short labels.
///
/// SipHash is the wrong tool for a name dictionary: it is built to resist a
/// chosen-input attacker at a cost the vocabulary does not need to pay on every
/// field of every document. This one is a multiply-xor over eight-byte words,
/// keyed with a per-instance random seed so that hash flooding needs the seed
/// and not just the algorithm. `label -> id` is the only direction that hashes
/// at all; `id -> label` is an index.
#[derive(Clone, Copy)]
struct FastBuild(u64);

impl Default for FastBuild {
    fn default() -> Self {
        use std::hash::BuildHasher;
        // One draw from the standard hasher to seed ours: still random per
        // process, still no dependency.
        let mut seed = std::collections::hash_map::RandomState::new().build_hasher();
        seed.write_u64(0x9e37_79b9_7f4a_7c15);
        FastBuild(seed.finish() | 1)
    }
}

impl BuildHasher for FastBuild {
    type Hasher = Fast;

    fn build_hasher(&self) -> Fast {
        Fast(self.0)
    }
}

struct Fast(u64);

impl Hasher for Fast {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let mut hash = self.0;
        let (words, remainder) = bytes.as_chunks::<8>();
        for word in words {
            hash = (hash.rotate_left(5) ^ u64::from_le_bytes(*word)).wrapping_mul(K);
        }
        let mut tail = 0u64;
        for (shift, byte) in remainder.iter().enumerate() {
            tail |= u64::from(*byte) << (8 * shift as u32);
        }
        self.0 = (hash.rotate_left(5) ^ tail ^ bytes.len() as u64).wrapping_mul(K);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The door a caller would write, with the store in a map: this crate never
    /// sees keys, so its own tests need none.
    #[derive(Default)]
    struct Mem {
        by_name: HashMap<String, u16>,
        by_id: HashMap<u16, String>,
        next: u16,
        /// Every allocation, so a test can report the commit it carried.
        written: Vec<u16>,
        committed: bool,
    }

    impl Names<u16> for Mem {
        type Error = ();

        fn scan(&mut self) -> Result<Vec<(String, u16)>, ()> {
            Ok(self.by_name.iter().map(|(n, i)| (n.clone(), *i)).collect())
        }
        fn name_id(&mut self, name: &str) -> Result<Option<u16>, ()> {
            Ok(self.by_name.get(name).copied())
        }
        fn id_name(&mut self, id: u16) -> Result<Option<String>, ()> {
            Ok(self.by_id.get(&id).cloned())
        }
        fn allocate(&mut self, name: &str) -> Result<u16, ()> {
            let id = self.next;
            self.next += 1;
            if self.committed {
                self.by_name.insert(name.to_owned(), id);
                self.by_id.insert(id, name.to_owned());
            } else {
                self.written.push(id);
            }
            Ok(id)
        }
    }

    #[test]
    fn a_hit_never_reaches_the_door() {
        struct Counting(u32);
        impl Names<u16> for Counting {
            type Error = ();
            fn scan(&mut self) -> Result<Vec<(String, u16)>, ()> {
                Ok(Vec::new())
            }
            fn name_id(&mut self, _: &str) -> Result<Option<u16>, ()> {
                self.0 += 1;
                Ok(None)
            }
            fn id_name(&mut self, _: u16) -> Result<Option<String>, ()> {
                self.0 += 1;
                Ok(None)
            }
            fn allocate(&mut self, _: &str) -> Result<u16, ()> {
                self.0 += 1;
                Ok(self.0 as u16)
            }
        }

        let mut vocab = Vocabulary::<u16>::new();
        let mut door = Counting(0);
        let id = vocab.ensure(&mut door, "addr").unwrap();
        vocab.landed(&[id]);
        let probes = door.0;
        assert_eq!(vocab.ensure(&mut door, "addr").unwrap(), id);
        assert_eq!(door.0, probes, "a landed hit must not probe");
        assert_eq!(
            vocab.name_of(&mut door, id).unwrap().as_deref(),
            Some("addr")
        );
        assert_eq!(door.0, probes, "the id direction is an index, not a probe");
    }

    #[test]
    fn a_miss_adopts_what_the_store_already_has() {
        let mut door = Mem {
            by_name: [("addr".to_string(), 7u16)].into(),
            by_id: [(7u16, "addr".to_string())].into(),
            next: 8,
            ..Mem::default()
        };
        let mut vocab = Vocabulary::<u16>::new();
        assert_eq!(vocab.ensure(&mut door, "addr").unwrap(), 7);
        assert_eq!(
            vocab.name_of(&mut door, 7).unwrap().as_deref(),
            Some("addr")
        );
        assert_eq!(door.written, Vec::<u16>::new(), "nothing was allocated");
    }

    #[test]
    fn an_unlanded_allocation_is_probed_again() {
        let mut door = Mem::default();
        let mut vocab = Vocabulary::<u16>::new();
        let first = vocab.ensure(&mut door, "addr").unwrap();
        assert_eq!(vocab.pending_len(), 1);
        // The caller's commit was refused: the store never got the entry, so a
        // second ensure must not trust the mirror's answer blindly.
        door.written.clear();
        let second = vocab.ensure(&mut door, "addr").unwrap();
        // The refused id is abandoned, not reused: ids are append-only, and the
        // row that named it was refused along with the vocabulary entry.
        assert_ne!(second, first);
        assert_eq!(vocab.pending_len(), 1, "one label, one pending entry");
        vocab.landed(&[second]);
        assert_eq!(vocab.pending_len(), 0);
    }

    #[test]
    fn init_merges_and_warms_both_directions() {
        let mut door = Mem {
            by_name: [("a".to_string(), 0u16), ("b".to_string(), 1u16)].into(),
            by_id: [(0u16, "a".to_string()), (1u16, "b".to_string())].into(),
            next: 2,
            ..Mem::default()
        };
        let mut vocab = Vocabulary::<u16>::new();
        vocab.init(&mut door).unwrap();
        assert_eq!(vocab.len(), 2);
        assert_eq!(vocab.id_of(&mut door, "b").unwrap(), Some(1));
        assert_eq!(vocab.name_of(&mut door, 0).unwrap().as_deref(), Some("a"));
        // A label allocated after the scan merges into the same tables.
        assert_eq!(vocab.ensure(&mut door, "c").unwrap(), 2);
        assert_eq!(vocab.len(), 3);
    }

    #[test]
    fn the_dense_direction_holds_many_ids_without_hashing() {
        let mut door = Mem::default();
        let mut vocab = Vocabulary::<u16>::with_capacity(1000);
        let mut ids = Vec::new();
        for i in 0..1000 {
            ids.push(vocab.ensure(&mut door, &format!("f{i:04}")).unwrap());
        }
        vocab.landed(&ids);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(
                vocab.name_of(&mut door, *id).unwrap(),
                Some(format!("f{i:04}"))
            );
        }
        assert_eq!(vocab.len(), 1000);
    }
}
