//! OKM — object-keyspace mapping 运行时
//!
//! 两个编码宏（KeyEncode / EdgeEncode）+ 一个组装点（Collection）。
//! feature: `fjall` / `slatedb` 提供对应引擎的 Collection 实现。

pub use okm_derive::{EdgeEncode, KeyEncode};

pub mod key;
pub mod edge;
pub mod engine;
pub mod collection;

#[cfg(feature = "fjall")]
pub mod fjall_backend;
#[cfg(feature = "slatedb")]
pub mod slatedb_backend;

pub use collection::Collection;
pub use edge::{KvEdge, head_bytes};
pub use engine::{KvEngine, MockStore};
pub use key::{KeyEncode, PrefixKey};

#[cfg(feature = "fjall")]
pub use fjall_backend::FjallStore;
#[cfg(feature = "slatedb")]
pub use slatedb_backend::{AsyncCollection, KvEngineAsync, SlatedbStore};
