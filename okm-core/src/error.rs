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

use okm_value::{Variant, dyn_kind};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Error {
    /// A name that matches a DECLARED field carries a `Variant` of another
    /// kind. Absence is NOT this error: an absent name takes the declared
    /// default (the append-only evolution rule).
    #[error("from_map: field `{field}` declares {want}; the map carries {got}")]
    KindMismatch {
        field: String,
        want: &'static str,
        got: &'static str,
    },
    /// A whole-document write was handed a payload that is not an object: a
    /// scalar or an array is a field VALUE, not a document, and there is no
    /// document-shaped reading of it. Refused before anything is written.
    #[error("a whole-document write takes an object; the payload is {got}")]
    NotADocument { got: &'static str },
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
    /// The store changed hands and the deployment was not told: the handover hook exited
    /// non-zero, could not be started, or ran out of budget (ADR-0056 §6).
    ///
    /// This is NOT a rolled-back cutover. The claim was taken before the hook ran, on
    /// purpose: an announcement that points traffic at an instance which cannot write is
    /// worse than a cutover nobody announced. The store belongs to this instance; what is
    /// missing is the routing.
    #[error(
        "handover hook `{program}` {what} — the store is already this instance's, nothing is undone"
    )]
    HandoverHook { program: String, what: String },
    /// A categorical label that cannot be interned: the vocabulary's id width
    /// is full. A wrapped id would name another label forever (ADR-0032).
    #[error(
        "vocabulary exhausted: id #{next} does not fit a {width}-byte vocabulary (ADR-0032) — \
         widen the field's N or model the value as a String field"
    )]
    VocabularyExhausted { width: usize, next: u128 },
    /// A nested object reached an encoder that owns no name vocabulary. The
    /// caller must encode through `okm_value::encode`, which can allocate.
    #[error(
        "nested object requires the vocabulary (name `{name}` unresolved) — use okm_value::encode"
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
    /// A declared node that no edge in the same write set names as an endpoint
    /// (ADR-0066 §3). A node exists only as an endpoint, so there is no
    /// "create a bare node" path — the declaration is refused rather than
    /// materializing a row nothing points at.
    #[error(
        "node {key} of ns {ns} is not an endpoint of any edge in this write — a node exists only as an endpoint (ADR-0066 §3)"
    )]
    BareNode { ns: u16, key: String },
    /// A cluster with fewer than two members. Such a set is not a grouping of
    /// anything, and the derivation of node membership is only total when the
    /// cluster is (ADR-0066 §3).
    #[error("a cluster needs two or more members; {kind}/{id} would have {members}")]
    ClusterTooSmall { kind: u8, id: u64, members: usize },
    /// `merge` / `split` / `reassign` named a cluster that no row holds and this
    /// write set does not create.
    #[error("cluster {kind}/{id} not found")]
    ClusterNotFound { kind: u8, id: u64 },
    /// `hyper_id` 0 named explicitly. The cluster row's parent head reserves `0`
    /// for "no parent" (ADR-0066 §4), so no cluster may BE that number — the
    /// runtime allocation starts at 1 for the same reason.
    #[error("hyper_id 0 is reserved for \"no parent\" (ADR-0066 §4); cluster ids start at 1")]
    ReservedClusterId,
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
    /// One ns holds one table, and the store already carries a different name at
    /// this ns. Two declarations sharing an ns share every face, every vocabulary
    /// cursor and the meta row itself, so the second claim is refused at attach
    /// instead of being half-served.
    ///
    /// The compile-time check (a name per ns, `E0428`) covers one module; this one
    /// covers the store, which is where declarations from different modules and
    /// different crates actually meet.
    #[error(
        "ns {ns} is already claimed by table `{existing}`; this declaration is `{declared}`. \
         One namespace is one table: give the second table its own ns, or mark the \
         second declaration #[ok_shared_ns] if the two are generations of one table"
    )]
    TableNameTaken {
        ns: u16,
        declared: String,
        existing: String,
    },
    /// The change log could not be opened or read (ADR-0045). Deliberately NOT
    /// on the write path: a log that cannot be appended records itself in
    /// `Collection::changelog_status` instead, because the write it describes is
    /// already committed and turning that into an error would report a failure
    /// the store did not have.
    #[error("changelog: {what}")]
    Changelog { what: String },
    /// A codec walk refused the bytes or the value tree (`CodecError`). The
    /// decoder and the window reader are core's now (ADR-0056 §8), so the leaf
    /// error has a home here instead of being stringified at the boundary.
    #[error("codec: {0}")]
    Codec(#[from] okm_value::CodecError),
}

impl Error {
    /// A present name whose kind does not match the declaration.
    pub fn kind_mismatch(field: impl Into<String>, want: &'static str, got: &Variant) -> Self {
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

/// The segment codec's failure, in this crate's vocabulary: a value too wide for
/// the table stays `Corrupt`, and a name-resolution failure is handed back as
/// the error it already was — never flattened into a string (ADR-0060 §6).
impl From<okm_value::Error<Error>> for Error {
    fn from(error: okm_value::Error<Error>) -> Self {
        match error {
            okm_value::Error::Corrupt { what } => Error::Corrupt { what },
            okm_value::Error::Names(inner) => inner,
        }
    }
}

/// The crate's result alias, so a caller writes `okm_core::Result<T>` and never
/// guesses whether a verb is fallible.
pub type Result<T> = std::result::Result<T, Error>;
