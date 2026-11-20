//! KV engine boundary: the [`storage::VirtualStorage`] abstraction and
//! its per-engine adapters, plus the remote nesting backend. Everything
//! engine-specific lives behind this module; the model layer sees only
//! the trait.

#[cfg(feature = "fjall")]
pub mod fjall_backend;
pub mod nest;
#[cfg(feature = "redb")]
pub mod redb_backend;
pub mod storage;

/// The ephemeral engine: okm's NON-PERSISTENT MODE — the same declarations over
/// an engine that keeps everything and promises nothing (ADR-0072). aura's
/// temporary realm is its first consumer; this crate's unit tests are a second,
/// for a cargo reason rather than a design one. A packed B+tree over chunked
/// tables, with no journal, no compaction and no snapshot — see the module docs
/// for the obligations that follow from that.
pub mod ephemeral;
