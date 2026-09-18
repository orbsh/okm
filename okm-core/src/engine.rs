//! KV engine boundary: the [`storage::VirtualStorage`] abstraction and
//! its per-engine adapters, plus the remote nesting backend. Everything
//! engine-specific lives behind this module; the model layer sees only
//! the trait.

pub mod nest;
pub mod storage;
#[cfg(any(feature = "test-engines", feature = "fjall", feature = "redb"))]
pub mod test_engine;
#[cfg(feature = "fjall")]
pub mod fjall_backend;
#[cfg(feature = "redb")]
pub mod redb_backend;
#[cfg(feature = "slatedb")]
pub mod slatedb_backend;
