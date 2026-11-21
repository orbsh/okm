//! okm-dynamic — schema-driven codec for embedded-language Actors
//! (ADR-0010 PLAN "dynamic codec").
//!
//! Host-independent core: given a [`CollectionSchema`] (exported from Rust by
//! `okm-core::schema::CollectionSchema::of`) and a plain value tree, encode
//! and decode the exact bytes the Rust derive would — cross-language
//! byte equality is locked by tests in the binding crates.
//!
//! Execution contract (Aura): Python/Steel implement a schema-driven
//! VirtualStorage — they encode/decode, the engine execution stays in
//! OKM (`StorageHost` executors, without a receiver prefix: same-shard
//! local execution, same code path for future sharded distribution; the
//! upper layer coordinates and orchestrates). Capability scope (ADR-0022):
//! subscribe stays excluded; reduce and function/partial indexes are
//! binding-implementable via host-language callables under the
//! deployment-shape contract (embedded: in-process calling discipline;
//! remote: operation payloads carry semantic results, single-writer-per-group).
//!
//! Value tree representation is host-agnostic (`okm_value::Variant`, re-exported
//! here as `Value`); binding
//! crates convert to/from their native types (vocab, hashmap, ...).
//!
//! Encoding rules (mirroring the derive, byte-for-byte):
//! - key fields: BE fixed-width at their static offsets, concatenated;
//! - payload: `[hot fields][cold TLV]` (no header: the hot width is the declaration's),
//!   cold frames `[tag u8][len u32 BE][value]` in declaration order;

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

#[cfg(feature = "arrow")]
pub mod arrow;
mod cluster;
mod collection;
mod graph;
mod plan;
mod write;

pub use okm_core::TableMeta;
// The two cluster spellings travel with the crate: a caller of the dynamic surface
// names them, and they are okm-core's types (one row, two relations).
pub use okm_core::field::FieldType;
pub use okm_core::schema::{CollectionSchema, FieldSchema};
pub use okm_core::{EdgeSet, NodeSet};
pub use okm_value::{CodecError, Dict, Variant as Value};

// NOTE (v1 scope): nested key segments are NOT supported by the dynamic
// codec — `FieldType::Segment` does not exist (the key-composition
// primitive was rejected; see PLAN). Key schemas here are flat primitive
// fields. Should segments return, the schema export already carries the
// FieldType variant slot for them.

/// The dynamic counterpart of core's graph + cluster vocabulary: the same four
/// cluster faces (ADR-0066) with `Dict` payloads, the same lazy traversal
/// streams, and the same three write verbs (`incorporate` / `retract` / `replace`).
pub use cluster::{
    Cluster, ClusterConflict, ClusterRef, ClusterRow, Cut, Decl, DerivedReport, DynFragment,
    EdgeConflict, HalfEdge, Kind, MEMBER_COUNT, NODE_COUNT, ReplaceReport, RetractReport,
    Selection, WriteReport, intern_kind, interned_kinds, interned_label,
};
pub use collection::DynamicCollection;
pub use graph::{DynEdge, DynEdgeStream, Graph, edge_face_prefix, edge_step, predicate_id_in};
pub use okm_core::decode::{decode_key, decode_payload, resolve_vocab_labels};
pub use okm_core::model::access_method::{
    AccessMethod, AccessMethodKind, Admits, FuncDerive, decode_includes, index_entries,
    path_index_entries, scan_access_method, scan_access_method_covered, scan_access_method_exact,
    scan_access_method_keys,
};
/// The name this trait had before the module moved into okm-core. Kept as an alias so an
/// implementation written against `okm_dynamic::ReduceLogic` keeps compiling; new code says
/// `ByteReduce`, which is what the trait is.
pub use okm_core::model::byte_reduce::ByteReduce as ReduceLogic;
pub use okm_core::model::byte_reduce::{
    BoundReduce, ByteReduce, ByteReduceObj, GroupAccs, PresetKind, PresetLogic, ReduceSpec,
    reduce_get, scan_reduces,
};
pub use okm_core::model::encode::{encode_fields, encode_key, encode_payload};
pub use okm_core::model::graph::{Direction, EdgeStep, HopHead};
pub use okm_core::model::path::{Component, Path};
pub use okm_core::model::plan_fields::{
    PlannedFields, VocabMirror, fields_from_frames, fields_from_frames_probing, fields_key,
    plan_delete_fields, plan_put_fields,
};
pub use plan::{PlannedDelete, PlannedPut, PlannedWrite, Stored, decode_stored};
pub use write::WritePayload;
