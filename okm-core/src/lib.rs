//! OKM — object-keyspace mapping runtime.
//!
//! OKM is the KV counterpart of ORM: derive macros map Rust structs onto
//! binary KV keys, turning schema correctness from a runtime database
//! concern into a compile-time guarantee. The macro layer is deliberately
//! storage-free — every `encode`/`decode` is a pure `Vec<u8>` in/out
//! function, and engine choice belongs to the assembly site.
//!
//! # Module layout
//!
//! ```text
//! model/    data model + codecs: keys, documents, indexes, junctions,
//!           reduces, schema, dynamic values, wrapper types
//! engine/   KV engine boundary: the VirtualStorage abstraction and its
//!           adapters (Fjall / redb / slatedb / TestStore) + remote nesting
//! bridge/   exports to the outside world (Arrow/Parquet, JSON schema,
//!           snapshots)
//! subscribe event emission (cross-cutting, stays top-level)
//! ```
//!
//! # Features
//!
//! - `fjall` — sync [`FjallStore`] engine adapter.
//! - `slatedb` — async [`SlatedbStore`] engine adapter (+ [`AsyncJunction`]).
//!
//! See `docs/adr/` for the design decisions (namespace dictionary,
//! secondary index slots, assembly point, frame codec) and the project
//! README for a full walkthrough.

pub use okm_derive::{JunctionEncode, EdgeEncode, KeyEncode, NestStorage, DocumentEncode};

pub mod model;
pub mod engine;
pub mod bridge;
pub mod subscribe;

// ================= re-exports (stable public surface) =================

pub use model::reverse::{Reversible, Reverse};
pub use model::wrappers::{Bytes, Ref, Refs, Enum, EnumTag, Offset, Quant, VarInt, VarIntEnc, Vector, VectorElem, offset_decode, offset_encode};
pub use model::wrappers::wire::{put_len, take_len};

pub use model::reduce::{reduce_get, scan_reduces, ReduceCodec, Reduce, ReduceLogic};
pub use model::presets::{Count, HighWater, LowAcc, LowWater, ReduceFieldSource, Sum};
pub use model::collection::Junction;
pub use model::junction::KvJunction;
pub use model::graph::{EdgeBody, EdgeFact, Graph, KvGraph, NodeRef};
pub use engine::storage::{VirtualStorage, KvBatch, MemBatch, SharedVirtualStorage};
#[cfg(any(feature = "test-engines", feature = "fjall", feature = "redb"))]
pub use engine::test_engine::TestStore;
pub use engine::nest::{NestStorage, RemoteStore, VirtualHandle};
pub use okm_wire::{OpFrame, OpResponse};
pub use model::field::{FieldDesc, FieldType};
pub use model::index::{Document, IndexFuncResult, IndexFuncValues, KvIndex, PRIMARY_SLOT, scan_index};
pub use model::key::{KeyEncode, PrefixKey};
pub use subscribe::{ChannelCell, Event, Op};
pub use model::document::Collection;

#[cfg(feature = "arrow")]
pub use bridge::arrow_bridge as arrow_backend;
#[cfg(feature = "parquet")]
pub use bridge::tooling::parquet_io;

#[cfg(feature = "fjall")]
pub use engine::fjall_backend::FjallStore;
#[cfg(feature = "slatedb")]
pub use engine::slatedb_backend::{AsyncJunction, VirtualStorageAsync, SlatedbStore};

// ================= path-compat re-exports =================
// Downstream crates (okm-dynamic, okm-query, bindings) and tests import
// these module paths; keep them valid while callers migrate.

pub use model::field as field;
pub use model::index as index;
pub use model::key as key;
pub use model::schema as schema;
pub use model::obj_dict as obj_dict;
pub use model::obj_dynamic as obj_dynamic;
pub use model::reduce as reduce;
pub use model::reverse as reverse;
pub use model::document as document;
pub use model::collection as collection;
pub use model::junction as junction;
pub use model::wrappers as wrappers;
pub use engine::storage as storage;
#[cfg(any(feature = "test-engines", feature = "fjall", feature = "redb"))]
pub use engine::test_engine as test_engine;
pub use engine::nest as nest;
#[cfg(feature = "fjall")]
pub use engine::fjall_backend as fjall_backend;
#[cfg(feature = "redb")]
pub use engine::redb_backend as redb_backend;
#[cfg(feature = "slatedb")]
pub use engine::slatedb_backend as slatedb_backend;
#[cfg(feature = "arrow")]
pub use bridge::arrow_bridge as arrow_bridge;
pub use bridge::tooling as tooling;

