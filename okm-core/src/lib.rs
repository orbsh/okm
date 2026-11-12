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
//! model/    data model + codecs: keys, documents, indexes, graphs,
//!           reduces, schema, dynamic values, wrapper types
//! engine/   KV engine boundary: the VirtualStorage abstraction and its
//!           adapters (Fjall / redb / localStorage) + remote nesting; engines that
//!           speak an async API live in `okm-async` (ADR-0059)
//! bridge/   exports to the outside world (Arrow/Parquet, JSON schema,
//!           snapshots)
//! subscribe event emission (cross-cutting, stays top-level)
//! ```
//!
//! # Features
//!
//! - `fjall` — sync [`FjallStore`] engine adapter.
//! - `redb` — sync [`RedbStore`] engine adapter.
//!
//! The engine whose own API is asynchronous lives in `okm-async` (ADR-0059),
//! together with the async mirror of the storage contract.
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

pub use okm_derive::{DocumentEncode, GraphEncode, KeyEncode, NestStorage};

pub mod engine;
pub mod error;
pub mod model;

pub use error::{Error, Result};
/// The change log (ADR-0045): append-only segment files written at the commit
/// boundary, drained out of process.
pub mod changelog;
/// The decoder: bytes + a declaration → the value tree (ADR-0056 §8). It lives
/// here because the tree is the value crate's (`okm_value`), and because the drain —
/// a core mechanism — reads payloads with it.
pub mod decode;
pub mod subscribe;

// ================= re-exports (stable public surface) =================

pub use model::reverse::{Reverse, Reversible};
pub use model::wrappers::wire::{prefix_width, put_len, take_len};
pub use model::wrappers::{
    Bytes, Offset, Quant, Ref, Refs, Uuid4, Uuid7, VarInt, VarIntEnc, Vector, VectorElem,
    dequantize, offset_decode, offset_encode, quantize, wire_to_be_bytes,
};

pub use engine::nest::{NestStorage, RemoteStore, VirtualHandle};
pub use engine::storage::{
    Batch, BatchStore, SharedVirtualStorage, VirtualStorage, scan_suffix_kv,
};
pub use model::document::{Collection, whole_document};
pub use model::field::{FieldDesc, FieldType};
pub use model::graph::{EdgeBody, EdgeHead, EdgeKey, Graph, GraphEncode, NodeRef, NodeSet};
pub use model::index::{
    DecodeIndexFields, Document, IndexFuncResult, KvIndex, LEAD_SLOT_BASE, META_SLOT, PRIMARY_SLOT,
    RawFields, RawHit, result_segments, scan_index, scan_index_exact, scan_raw, slot_len,
    write_slot,
};
pub use model::key::{
    KeyEncode, KeyFieldValue, PrefixKey, key_field_exists, key_field_matches, kind_matches,
};
pub use model::parts;
pub use model::parts::{Fields, Hit, Include, Key, Part, Parts, PointParts, Row, Value};
pub use model::prefix::{IndexProbe, PrefixProbe, ProbeBytes};
pub use model::presets::{Count, HighWater, LowAcc, LowWater, ReduceFieldSource, Sum};
pub use model::reduce::{Reduce, ReduceCodec, ReduceLogic, reduce_get, scan_reduces};
pub use model::vocab::Vocab;
pub use model::write::WritePayload;
pub use okm_value::Id;
pub use okm_wire::{OP_DELETE, OP_PUT, OpFrame, OpResponse};
pub use subscribe::{Event, Op, SinkCell};

#[cfg(feature = "fjall")]
pub use engine::fjall_backend::FjallStore;

// ================= path-compat re-exports =================
// Downstream crates (okm-dynamic, okm-query, bindings) and tests import
// these module paths; keep them valid while callers migrate.

#[cfg(feature = "fjall")]
pub use engine::fjall_backend;
pub use engine::nest;
#[cfg(feature = "redb")]
pub use engine::redb_backend;
pub use engine::storage;
pub use model::document;
pub use model::faces::{FINGERPRINT_LEN, FaceSpec, IndexDecl, RebuildReport, fingerprint_specs};
pub use model::field;
pub use model::index;
pub use model::key;
pub use model::meta;
pub use model::meta::{FaceStatus, TableMeta};
// The value crate's own name, so generated code can write
// `::okm_core::okm_value::DynamicValue` and a user of THIS crate never has to
// add `okm-value` to their own manifest. Not an alias of an old path: it is the
// crate, re-exported.
pub use model::obj_vocab;
pub use model::reduce;
pub use model::reverse;
pub use model::schema;
pub use model::wrappers;
pub use okm_value;
