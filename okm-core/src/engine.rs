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

/// The in-memory engine: a `BTreeMap` behind a mutex. Published because it is
/// the store a host's memory mode binds to (aura's `Engine::Memory`) — a host
/// must not grow its own stand-in — and, separately, the store this crate's
/// unit tests run on (see the module docs for why the test side cannot use the
/// real-engine matrix).
pub mod mem_store;
