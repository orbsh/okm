//! One write verb, the payload type chooses the path (ADR-0035 as amended).
//!
//! A `Collection` used to carry three write verbs whose difference was the
//! SHAPE of the payload — `put(&R)` for a typed row, `merge(&map)` for a
//! partial document, `replace(&map)` for a whole one — so a caller had to
//! translate its data into a verb name before it could write, and the three
//! names carried three different face lifecycles. The lifecycle follows from
//! the payload, not from the spelling: a typed row is the fresh-row fast path
//! (no read, no cleanup), a nameless partial map has to read the stored row to
//! sweep the faces it supersedes, and a whole-map write is the same read with
//! the opposite rule for absent names. So the verb is one and the payload
//! selects the path.
//!
//! The tuple is the "whole thing" marker: `(&Variant,)` is a one-element
//! tuple, deliberately, because a bare `&Variant` already means "the names
//! here" and one wrapper is the smallest thing that cannot be confused with it.
//!
//! `&R` and `&Variant` cannot both match one call: the impls are on
//! distinct types, and a document type is not itself a `Variant`.

use okm_value::Variant;

use crate::engine::storage::VirtualStorage;
use crate::model::document::Collection;
use crate::model::index::Document;
use crate::model::key::KeyEncode;

/// What a `put` writes, and therefore which path it takes.
///
/// Implemented for the three payload shapes a document write accepts:
///
/// - `&R` — a typed row: the struct door, today's `put` contract (primary
///   entry + declared faces + embedded children + reduce RMWs; the previous
///   row's faces are NOT swept — that is `purge`'s and `save_with_prev_into`'s
///   job).
/// - `&Variant` — a partial document, which must be
///   [`Variant::Map`]: declared names read-modify-write the row and sweep
///   the faces they supersede, every other name overwrites itself in the
///   dynamic segment and leaves its siblings alone.
/// - `(&Variant,)` — the whole document: the map IS the document, so a
///   declared name absent from it takes the declaration's default and an
///   unnamed dynamic field is removed.
///
/// A scalar or array payload is not a document: it is a field VALUE, and the
/// write is refused with [`Error::NotADocument`](crate::error::Error::NotADocument)
/// before anything reaches the store.
pub trait WritePayload<S, K, R>
where
    S: VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
{
    /// Write this payload at `key` through `col`'s write path.
    fn write(self, col: &Collection<S, K, R>, key: &K) -> crate::error::Result<()>;
}

/// Typed row: the struct door (`put`'s original contract, face cleanup
/// included in the "none").
impl<S, K, R> WritePayload<S, K, R> for &R
where
    S: VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
{
    fn write(self, col: &Collection<S, K, R>, key: &K) -> crate::error::Result<()> {
        col.put_typed(key, self)
    }
}

/// Partial document: the merge path. Only an object is a document.
impl<S, K, R> WritePayload<S, K, R> for &Variant
where
    S: VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
{
    fn write(self, col: &Collection<S, K, R>, key: &K) -> crate::error::Result<()> {
        match self {
            Variant::Map(object) => col.merge_map(key, object),
            other => Err(crate::error::Error::NotADocument {
                got: okm_value::dyn_kind(other),
            }),
        }
    }
}

/// Whole document: the replace path, marked by the one-element tuple.
impl<S, K, R> WritePayload<S, K, R> for (&Variant,)
where
    S: VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
{
    fn write(self, col: &Collection<S, K, R>, key: &K) -> crate::error::Result<()> {
        match self.0 {
            Variant::Map(object) => col.replace_map(key, object),
            other => Err(crate::error::Error::NotADocument {
                got: okm_value::dyn_kind(other),
            }),
        }
    }
}
