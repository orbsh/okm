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
//! See `docs/adr/` for the design decisions (namespace vocabulary,
//! secondary index slots, assembly point, frame codec) and the project
//! README for a full walkthrough.

// Tests keep the vocabulary: a panicking test is a failing test with a stack trace
// (and the oracle for every failable path is "assert on the error VALUE"). This line
// is per-crate because `[lints]` is per-package and covers every target, with no way
// to scope a lint to lib/bins — the manifest cannot express `cfg(test)`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]

pub use okm_derive::{DocumentEncode, GraphEncode, JunctionEncode, KeyEncode, NestStorage};

pub mod engine;
pub mod error;
pub mod model;

pub use error::{Error, Result};
pub mod bridge;
/// The change log (ADR-0045): append-only segment files written at the commit
/// boundary, drained out of process.
pub mod changelog;
pub mod subscribe;

// ================= re-exports (stable public surface) =================

pub use model::reverse::{Reverse, Reversible};
pub use model::wrappers::wire::{prefix_width, put_len, take_len};
pub use model::wrappers::{
    Bytes, Offset, Quant, Ref, Refs, Uuid4, Uuid7, VarInt, VarIntEnc, Vector, VectorElem,
    dequantize, offset_decode, offset_encode, quantize, wire_to_be_bytes,
};

/// One exported batch — re-exported so a consumer can pass the graph's edge
/// table to `parquet_io::write_batch` without taking an Arrow dependency of its
/// own (ADR-0034).
#[cfg(feature = "parquet")]
pub use arrow::array::RecordBatch;
pub use engine::nest::{NestStorage, RemoteStore, VirtualHandle};
pub use engine::storage::{
    Batch, BatchStore, SharedVirtualStorage, VirtualStorage, VirtualStorageAsync, scan_suffix_kv,
    scan_suffix_kv_async,
};
#[cfg(any(feature = "test-engines", feature = "fjall", feature = "redb"))]
pub use engine::test_engine::TestStore;
pub use engine::wire_client::{WireClient, WireTransport};
pub use model::collection::Junction;
pub use model::document::Collection;
pub use model::field::{FieldDesc, FieldType};
pub use model::graph::{EdgeBody, EdgeHead, EdgeKey, Graph, GraphEncode, NodeRef, NodeSet};
pub use model::index::{
    Document, IndexFuncResult, IndexFuncValues, KvIndex, META_SLOT, PRIMARY_SLOT, scan_index,
    scan_index_exact, slot_len, write_slot,
};
pub use model::junction::KvJunction;
pub use model::key::{KeyEncode, PrefixKey};
pub use model::presets::{Count, HighWater, LowAcc, LowWater, ReduceFieldSource, Sum};
pub use model::reduce::{Reduce, ReduceCodec, ReduceLogic, reduce_get, scan_reduces};
pub use model::vocab::{Vocab, VocabId};
pub use okm_wire::{OP_DELETE, OP_PUT, OpFrame, OpResponse};
pub use subscribe::{Event, Op, SinkCell};

#[cfg(feature = "arrow")]
pub use bridge::arrow_bridge as arrow_backend;
#[cfg(feature = "parquet")]
pub use bridge::tooling::parquet_io;

#[cfg(feature = "fjall")]
pub use engine::fjall_backend::FjallStore;
#[cfg(feature = "localstorage")]
pub use engine::localstorage_backend::{LocalStorageStore, b64_decode, b64_encode};
#[cfg(feature = "slatedb")]
pub use engine::slatedb_backend::{AsyncJunction, SlatedbStore};

// ================= path-compat re-exports =================
// Downstream crates (okm-dynamic, okm-query, bindings) and tests import
// these module paths; keep them valid while callers migrate.

#[cfg(feature = "arrow")]
pub use bridge::arrow_bridge;
pub use bridge::tooling;
#[cfg(feature = "fjall")]
pub use engine::fjall_backend;
#[cfg(feature = "localstorage")]
pub use engine::localstorage_backend;
pub use engine::nest;
#[cfg(feature = "redb")]
pub use engine::redb_backend;
#[cfg(feature = "slatedb")]
pub use engine::slatedb_backend;
pub use engine::storage;
#[cfg(any(feature = "test-engines", feature = "fjall", feature = "redb"))]
pub use engine::test_engine;
pub use model::collection;
pub use model::document;
pub use model::faces::{FINGERPRINT_LEN, IndexDecl, RebuildReport};
pub use model::field;
pub use model::index;
pub use model::junction;
pub use model::key;
pub use model::meta;
pub use model::meta::{FaceStatus, TableMeta};
pub use model::obj_dynamic;
pub use model::obj_vocab;
pub use model::reduce;
pub use model::reverse;
pub use model::schema;
pub use model::wrappers;
