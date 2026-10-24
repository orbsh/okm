//! OKM's error type: what the map-facing write verbs can REFUSE.
//!
//! The library has two failure kinds, and the difference is who can act on it:
//!
//! - **Caller data** ([`Error::KindMismatch`], [`Error::ValueOutOfRange`]): a
//!   name-keyed map does not fit the declared document. That is normal input —
//!   the shape a request body or a decoded row arrives in — so it comes back as
//!   a value the caller handles, never as a panic.
//! - **Engine** ([`Error::Engine`]): the store refused the commit (a fenced
//!   writer, an I/O failure, a remote frame error). The engine's own text rides
//!   inside: the layer above needs the engine's words, not a generic message.
//!
//! Only the map-facing verbs are fallible today (`merge`, `replace`,
//! `Document::try_from_map`). A typed verb (`put`, `purge`) receives a value
//! that is already typed, so its only possible failure is the engine's — that
//! path still panics, which is the single-writer in-process contract recorded in
//! ADR-0035 and `docs/CONCURRENCY.md`. Widening it is a separate decision with
//! its own surface cost (every call site in every consumer); the error type
//! below is what that decision would hang on.

use thiserror::Error;

use crate::model::obj_dynamic::{dyn_kind, DynamicValue};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Error {
    /// A name that matches a DECLARED field carries a `DynamicValue` of another
    /// kind. Absence is NOT this error: an absent name takes the declared
    /// default (the append-only evolution rule).
    #[error("from_map: field `{field}` declares {want}; the map carries {got}")]
    KindMismatch {
        field: String,
        want: &'static str,
        got: &'static str,
    },
    /// Right kind, unrepresentable value — a number too wide for the declared
    /// field, a byte string of the wrong length. Truncating it would invent
    /// data just as surely as defaulting it.
    #[error(
        "from_map: field `{field}` declares {want}; the map carries {got}, which cannot be represented"
    )]
    ValueOutOfRange {
        field: String,
        want: &'static str,
        got: String,
    },
    /// A categorical label that cannot be interned: the vocabulary's id width
    /// is full. A wrapped id would name another label forever (ADR-0032).
    #[error(
        "vocabulary exhausted: id #{next} does not fit a {width}-byte vocabulary (ADR-0032) — \
         widen the field's N or model the value as a String field"
    )]
    VocabularyExhausted { width: usize, next: u128 },
    /// A nested object reached an encoder that owns no name vocabulary. The
    /// caller must use the named path (`put_frame_named`), which can allocate.
    #[error(
        "nested object requires the vocabulary (name `{name}` unresolved) — use put_frame_named"
    )]
    VocabularyRequired { name: String },
    /// `Graph::link` on an id that is already live — identity stays with the
    /// caller, so a collision is the caller's bug, not a retry.
    #[error("edge_id {edge_id} already live")]
    EdgeAlreadyLive { edge_id: u64 },
    /// `Graph::unlink` on an id with no stored edge.
    #[error("unlink: edge_id {edge_id} not found")]
    EdgeNotFound { edge_id: u64 },
    /// An edge endpoint whose namespace is not a member of the graph's.
    #[error("{side} ns {ns} is not a member of graph ns {graph_ns}")]
    EdgeNotMember {
        side: &'static str,
        ns: u16,
        graph_ns: u16,
    },
    /// The engine refused the commit; the text is the engine's own.
    #[error("engine commit failed: {0}")]
    Engine(String),
}

impl Error {
    /// A present name whose kind does not match the declaration.
    pub fn kind_mismatch(
        field: impl Into<String>,
        want: &'static str,
        got: &DynamicValue,
    ) -> Self {
        Self::KindMismatch {
            field: field.into(),
            want,
            got: dyn_kind(got),
        }
    }

    /// A present value that cannot be represented in the declared field.
    pub fn bad_value(
        field: impl Into<String>,
        want: &'static str,
        got: impl std::fmt::Display,
    ) -> Self {
        Self::ValueOutOfRange {
            field: field.into(),
            want,
            got: got.to_string(),
        }
    }
}

/// The crate's result alias, so a caller writes `okm_core::Result<T>` and never
/// guesses whether a verb is fallible.
pub type Result<T> = std::result::Result<T, Error>;