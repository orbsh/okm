//! What a scan takes out of an entry.
//!
//! An entry carries four things: the field segment it is ordered by, the key its
//! tail holds, the document behind it, and the `includes` payload in its value.
//! Which of those a call wants is a property of the QUESTION, not of the access
//! method — so it is the type parameter of every scan verb: a tuple of [`Part`]s,
//! in the caller's order.
//!
//! ```ignore
//! t.scan::<ByOrg>(&probe)                                       // (Fields, Key, Row)
//! t.scan::<ByOrg>(&probe).parts::<(parts::Key, parts::Fields)>() // no fetch, order as written
//! ```
//!
//! Both the order and the number of parts are the caller's: `(Fields, Key)` and
//! `(Key, Fields)` are different questions, and asking for fewer parts is how a
//! caller skips work. A part that is not in the tuple is not in the monomorphized
//! body — no branch, no `Option`, nothing skipped at run time because nothing was
//! generated.
//!
//! The arity is bounded by the tuple impls below: one per arity, and the ceiling is
//! the number of parts (four), because a fifth slot could only repeat one of them.
//! That is a language limit, not a design one — Rust tuples are not variadic, so N
//! positions need N impls — and it costs four impls for every order and every
//! selection: there is still exactly ONE scan method.

use crate::engine::storage::VirtualStorage;
use crate::error::Result;
use crate::model::document::Collection;
use crate::model::index::{Document, KvIndex, RawHit};
use crate::model::key::KeyEncode;

/// How a part reaches the row: one point read, handed in by the scan verb so a
/// part never has to know which collection it is reading.
pub type Fetch<'a, I> =
    &'a mut dyn FnMut(&<I as KvIndex>::Key) -> Result<Option<<I as KvIndex>::Document>>;
use crate::model::key::PrefixKey;

/// One entry, before any part is taken: the bytes, plus the ability to fetch the
/// row. Built by the scan verbs; parts read it and never mutate the entry.
pub struct Hit<'a, I: KvIndex> {
    raw: &'a RawHit,
    /// Decoded on demand: a caller that asked for neither `Key` nor `Row` never
    /// pays for it.
    key: std::cell::OnceCell<PrefixKey<I::Key>>,
    fetch: Fetch<'a, I>,
}

impl<'a, I: KvIndex> Hit<'a, I> {
    pub fn new(raw: &'a RawHit, fetch: Fetch<'a, I>) -> Self {
        Self {
            raw,
            key: std::cell::OnceCell::new(),
            fetch,
        }
    }

    /// The sort segment, whole: the bytes the entry was ordered by.
    pub fn segment(&self) -> &'a [u8] {
        &self.raw.segment
    }

    /// The key the tail holds, decoded once per hit. Past a truncation boundary
    /// the trailing fields are zero-filled garbage — `taken` says how much is
    /// trustworthy (see [`PrefixKey`]).
    pub fn key(&self) -> &PrefixKey<I::Key> {
        self.key.get_or_init(|| {
            let taken = self.raw.tail.len();
            let kl = I::Key::KEY_LEN;
            let decoded = if taken == kl {
                I::Key::decode(&self.raw.tail)
            } else {
                let mut buf = vec![0u8; kl];
                buf[..taken].copy_from_slice(&self.raw.tail);
                I::Key::decode(&buf)
            };
            PrefixKey { decoded, taken }
        })
    }

    /// The entry value: `includes` bytes, or empty.
    pub fn value(&self) -> &'a [u8] {
        &self.raw.value
    }

    /// The row behind this entry — one point read, and only when a part asks. A
    /// truncated key cannot be fetched back (`None`, not an error: the entry is a
    /// legitimate hit, its row is simply not addressable from here).
    pub fn row(&mut self) -> Result<Option<I::Document>> {
        let key = self.key().clone();
        if key.taken != I::Key::KEY_LEN {
            return Ok(None);
        }
        (self.fetch)(&key.decoded)
    }
}

/// One answer a scan can give about a hit.
pub trait Part<I: KvIndex> {
    /// What this part contributes to the tuple the caller asked for.
    type Output;
    fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output>;
}

/// The field segment, decoded — the column(s) the entry was ordered by.
pub struct Fields;

impl<I: KvIndex> Part<I> for Fields {
    type Output = I::Fields;
    fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output> {
        Ok(I::decode_fields(hit.segment()))
    }
}

/// The key the entry's tail carries.
pub struct Key;

impl<I: KvIndex> Part<I> for Key {
    type Output = PrefixKey<I::Key>;
    fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output> {
        Ok(hit.key().clone())
    }
}

/// The document behind the entry — the only part that reads the store.
pub struct Row;

impl<I: KvIndex> Part<I> for Row {
    type Output = Option<I::Document>;
    fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output> {
        hit.row()
    }
}

/// The `includes` payload: the value the entry already carries, no extra read.
pub struct Include;

impl<I: KvIndex> Part<I> for Include {
    type Output = I::Covered;
    fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output> {
        Ok(I::decode_covered(hit.value()))
    }
}

/// A tuple of [`Part`]s, in the caller's order — the payload half of a scan verb.
pub trait Parts<I: KvIndex> {
    type Output;
    fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output>;
}

/// Ask for nothing at all: `scan::<I, ()>(probe).count()` is how many entries the
/// question has, with no decode and no fetch. The empty tuple is the zero-arity
/// case of the same menu.
impl<I: KvIndex> Parts<I> for () {
    type Output = ();
    fn take(_hit: &mut Hit<'_, I>) -> Result<Self::Output> {
        Ok(())
    }
}

macro_rules! parts_for_tuple {
    ($($name:ident),+) => {
        impl<I: KvIndex, $($name: Part<I>),+> Parts<I> for ($($name,)+) {
            type Output = ($($name::Output,)+);
            fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output> {
                Ok(($($name::take(hit)?,)+))
            }
        }
    };
}

// Four arities, not eight: there are four parts, so a wider tuple could only
// repeat one of them. Every order and every selection within an arity comes out of
// the same generic impl — 64 ordered choices cost four impls, not 64.
parts_for_tuple!(A);
parts_for_tuple!(A, B);
parts_for_tuple!(A, B, C);
parts_for_tuple!(A, B, C, D);

// ===================== point reads =====================
//
// A scan walks entries; a point read has no entry, only a key and the two slots
// behind it (ADR-0035: a document is the PAIR). So the same idea, one level
// down: the caller names the parts it wants and pays for exactly those.

/// The dynamic segment (slot 1) as a name-keyed object — the half of the
/// document that has no struct field. Point reads only: a scan entry carries
/// `includes` bytes, not the dynamic segment, so there is nothing here for a
/// [`Part`] to do.
pub struct Value;

/// What a point read answers about one key.
///
/// Each part is its OWN `Option`, because absence is per slot: a key holding
/// only a dynamic segment answers `(None, Some(object))`, and a key holding
/// nothing answers `(None, None)`. Merging them into one document is the
/// caller's composition — the row's [`to_value`](crate::model::index::Document::to_value)
/// merged with this object — so a caller that names one half never
/// materializes the other.
///
/// One-element tuples collapse to the part's own output — `(parts::Row,)` is
/// `Option<R>`, not `(Option<R>,)` — so the sugar [`Collection::get_row`] and
/// the general form answer the same type.
pub trait PointParts<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>> {
    /// What this selection contributes to the caller's answer.
    type Output;
    fn take(col: &Collection<S, K, R>, key: &K) -> Result<Self::Output>;
}

impl<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>> PointParts<S, K, R> for Row {
    type Output = Option<R>;
    fn take(col: &Collection<S, K, R>, key: &K) -> Result<Self::Output> {
        col.get_row(key)
    }
}

impl<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>> PointParts<S, K, R> for Value {
    type Output = Option<okm_value::Variant>;
    fn take(col: &Collection<S, K, R>, key: &K) -> Result<Self::Output> {
        Ok(col.slot1_map(key)?.map(okm_value::Variant::Map))
    }
}

/// The collapse rule: one part named is that part's answer, unwrapped from the
/// tuple. Two and three parts are the tuple of their answers, in call order.
impl<S, K, R, A> PointParts<S, K, R> for (A,)
where
    S: VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
    A: PointParts<S, K, R>,
{
    type Output = A::Output;
    fn take(col: &Collection<S, K, R>, key: &K) -> Result<Self::Output> {
        A::take(col, key)
    }
}

macro_rules! point_parts_for_tuple {
    ($($name:ident),+) => {
        impl<S, K, R, $($name),+> PointParts<S, K, R> for ($($name,)+)
        where
            S: VirtualStorage,
            K: KeyEncode,
            R: Document<Key = K>,
            $($name: PointParts<S, K, R>),+
        {
            type Output = ($($name::Output,)+);
            fn take(col: &Collection<S, K, R>, key: &K) -> Result<Self::Output> {
                Ok(($($name::take(col, key)?,)+))
            }
        }
    };
}

// Two halves, so two is the useful ceiling; three costs one more impl and keeps
// a caller from having to invent a struct for a third question.
point_parts_for_tuple!(A, B);
point_parts_for_tuple!(A, B, C);
