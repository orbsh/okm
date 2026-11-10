//! KV engine boundary: the [`storage::VirtualStorage`] abstraction and
//! its per-engine adapters, plus the remote nesting backend. Everything
//! engine-specific lives behind this module; the model layer sees only
//! the trait.

#[cfg(feature = "fjall")]
pub mod fjall_backend;
#[cfg(feature = "localstorage")]
pub mod localstorage_backend;
pub mod nest;
#[cfg(feature = "redb")]
pub mod redb_backend;
pub mod storage;

/// The store this crate's UNIT tests run on (see the module docs for why it is
/// not the real-engine matrix: a dev-dependency that leads back here makes
/// cargo build this package twice, and the two copies' traits differ).
#[cfg(test)]
pub mod mem_store;
