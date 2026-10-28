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
//! Every verb that touches a store is fallible now, including the typed ones:
//! `put` / `purge` answer the engine's refusal rather than aborting (ADR-0039),
//! and the storage trait under them does the same. What still aborts is the
//! CODEC — bytes this SDK did not write cannot be decoded back into a document,
//! and a half-true row would corrupt the derived state written from it
//! (ADR-0040, which also lists where the panic lints are allowed).
//!
//! The enum is `#[non_exhaustive]`: this crate's failure surface has grown once
//! per decision (ADR-0038 gave reads a shape, ADR-0039 writes), and a new variant
//! must be a compile-time no-op for everyone downstream rather than a breaking
//! change. Matching it from another crate costs one `_ =>` arm.

use thiserror::Error;

use crate::model::obj_dynamic::{dyn_kind, DynamicValue};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
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
    /// The store's meta entry is stamped with another payload shape (ADR-0043),
    /// or names no version at all. Not a compatibility failure and not a
    /// migration prompt: the shape this build writes is the only one it reads,
    /// so the store is exported by the build that wrote it (or deleted) and the
    /// rows come back through import (ADR-0042).
    #[error(
        "table layout mismatch: the store is stamped {stored:?}, this build writes {current} — \
         export the store with the build that stamped it, or delete it (ADR-0042)"
    )]
    LayoutMismatch { stored: Option<u8>, current: u8 },
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
    /// Stored bytes that the declaration says cannot exist (a primary entry
    /// shorter than its own key). The store is broken, not the caller — but a
    /// read verb reports it instead of aborting the process.
    #[error("{what}")]
    Corrupt { what: String },
    /// A scan was issued through a `BatchStore`. A batch's pending ops and the
    /// store's iterator cannot be merged into one ordered view, so this is a
    /// caller mistake rather than a store condition — reported, never hidden.
    #[error(
        "a BatchStore refuses scans: a batch's pending ops and the store's iterator cannot be          merged into one ordered view. Return to the store, read what you need, then open the          batch"
    )]
    ScanThroughBatch,
    /// The engine (or the transport to one) refused the operation; the text
    /// is the engine's own. Reads reach this variant too — a failure is not
    /// a commit-shaped event only.
    #[error("{0}")]
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