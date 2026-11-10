//! The asynchronous face of okm's storage contract (ADR-0059).
//!
//! Core's contract is synchronous — `okm_core::engine::storage::VirtualStorage`
//! has no `async fn`. Everything that speaks the other one lives here: the six
//! method mirror ([`VirtualStorageAsync`]), the sender that rides a byte
//! transport ([`WireClient`]), and the engine whose own API is asynchronous
//! ([`SlatedbStore`], driven back down to the sync contract by [`SlatedbSync`]).
//!
//! The dependency points one way (`okm-async → okm-core`) and nothing in core
//! names this crate. That is the point of the split: cargo enforces a crate
//! boundary where a feature flag only advertises one, so a build that does not
//! depend on this crate has no async surface, no `slatedb`, and no runtime.
//!
//! Running this crate's own tests needs an engine: `cargo test -p okm-async
//! --features slatedb`. `slatedb` is off by default so that a consumer wanting
//! only the mirror is not handed tokio.

pub mod storage;
#[cfg(any(feature = "slatedb", feature = "fjall", feature = "redb"))]
pub mod test_engine;
pub mod wire_client;

#[cfg(feature = "slatedb")]
pub mod slatedb_backend;

pub use storage::{VirtualStorageAsync, scan_suffix_kv_async};
#[cfg(any(feature = "slatedb", feature = "fjall", feature = "redb"))]
pub use test_engine::TestStore;
pub use wire_client::{WireClient, WireTransport};

#[cfg(feature = "slatedb")]
pub use slatedb_backend::{SlatedbStore, SlatedbSync};
