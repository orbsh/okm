//! OKM — object-keyspace mapping runtime.
//!
//! OKM is the KV counterpart of ORM: derive macros map Rust structs onto
//! binary KV keys, turning schema correctness from a runtime database
//! concern into a compile-time guarantee. The macro layer is deliberately
//! storage-free — every `encode`/`decode` is a pure `Vec<u8>` in/out
//! function, and engine choice belongs to the assembly site.
//!
//! # Architecture
//!
//! ```text
//! KeyEncode / EdgeEncode derive macros   ← compile-time codecs, zero I/O
//!         ↓ expand into pure functions
//! Collection<S, E>                       ← assembly point: engine + edge type
//!         ↓ trait dispatch
//! VirtualStorage (FjallStore / SlatedbStore / TestStore)  ← real storage lives here
//! ```
//!
//! # Features
//!
//! - `fjall` — sync [`FjallStore`] engine adapter.
//! - `slatedb` — async [`SlatedbStore`] engine adapter (+ [`AsyncCollection`]).
//!
//! See `docs/adr/` for the design decisions (direction-bit niche, namespace
//! dictionary, assembly point, value-side roadmap) and the project README
//! for a full walkthrough.

pub use okm_derive::{JunctionEncode, KeyEncode, NestStorage, DocumentEncode};

pub use reverse::{Reversible, Reverse};
pub use wrappers::{Ref, Refs, Enum, EnumTag, Offset, Quant, VarInt, VarIntEnc, Vector, VectorElem, offset_decode, offset_encode};

pub mod reduce;
pub mod collection;
pub mod junction;
pub mod nest;
pub mod obj_dict;
pub mod obj_dynamic;
pub mod schema;
#[cfg(feature = "redb")]
pub mod redb_backend;
pub mod storage;
#[cfg(any(feature = "test-engines", feature = "fjall", feature = "redb"))]
pub mod test_engine;
pub mod field;
pub mod index;
pub mod key;
pub mod reverse;
pub mod subscribe;
pub mod document;
pub mod wrappers;

#[cfg(feature = "arrow")]
pub mod arrow_bridge;
#[cfg(feature = "fjall")]
pub mod fjall_backend;
#[cfg(feature = "slatedb")]
pub mod slatedb_backend;
pub mod tooling;

pub use reduce::{reduce_get, scan_reduces, ReduceCodec, Reduce, ReduceLogic};
pub use collection::Junction;
pub use junction::KvJunction;
pub use storage::{VirtualStorage, KvBatch, MemBatch, SharedVirtualStorage};
#[cfg(any(feature = "test-engines", feature = "fjall", feature = "redb"))]
pub use test_engine::TestStore;
pub use nest::{NestStorage, RemoteStore, VirtualHandle};
pub use okm_wire::{OpFrame, OpResponse};
pub use field::{FieldDesc, FieldType};
pub use index::{Document, IndexFuncResult, IndexFuncValues, KvIndex, PRIMARY_SLOT, scan_index};
pub use key::{KeyEncode, PrefixKey};
pub use subscribe::{ChannelCell, Event, Op};
pub use document::Collection;

#[cfg(feature = "arrow")]
pub use arrow_bridge as arrow_backend;
#[cfg(feature = "parquet")]
pub use tooling::parquet_io;

#[cfg(feature = "fjall")]
pub use fjall_backend::FjallStore;
#[cfg(feature = "slatedb")]
pub use slatedb_backend::{AsyncJunction, VirtualStorageAsync, SlatedbStore};
