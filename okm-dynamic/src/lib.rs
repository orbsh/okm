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
mod collection;
mod encode;
mod graph;
mod index;
mod path;
mod plan;
mod plan_fields;
mod reduce;
mod write;

pub use okm_core::TableMeta;
pub use okm_core::field::FieldType;
pub use okm_core::schema::{CollectionSchema, FieldSchema};
pub use okm_value::{CodecError, Dict, Variant as Value};

// NOTE (v1 scope): nested key segments are NOT supported by the dynamic
// codec — `FieldType::Segment` does not exist (the key-composition
// primitive was rejected; see PLAN). Key schemas here are flat primitive
// fields. Should segments return, the schema export already carries the
// FieldType variant slot for them.

pub use collection::DynamicCollection;
pub use encode::{encode_fields, encode_key, encode_payload};
pub use graph::{DynEdge, Graph};
pub use index::{
    AccessMethod, AccessMethodKind, Admits, FuncDerive, decode_includes, index_entries,
    path_index_entries, scan_access_method, scan_access_method_covered, scan_access_method_exact,
};
pub use okm_core::decode::{decode_key, decode_payload, resolve_vocab_labels};
pub use path::{Component, Path};
pub use plan::{PlannedDelete, PlannedPut, PlannedWrite, Stored, decode_stored};
pub use plan_fields::{
    PlannedFields, VocabMirror, fields_from_frames, fields_from_frames_probing, fields_key,
    plan_delete_fields, plan_put_fields,
};
pub use reduce::{
    BoundReduce, PresetKind, PresetLogic, ReduceLogic, ReduceLogicObj, ReduceSpec, reduce_get,
    scan_reduces,
};
pub use write::WritePayload;

/// Look up a field's value, rejecting unknown/missing names up front —
/// both sides of every encode walk this gate, so byte layout and error
/// reporting stay in one place.
pub(crate) fn field_pair<'a>(
    field: &okm_core::schema::FieldSchema,
    map: &'a Dict,
) -> Result<&'a Value, CodecError> {
    let name = field.name.as_str();
    let schema_ty = field.ty;
    let v = map
        .get(name)
        .ok_or_else(|| CodecError::MissingField(name.to_string()))?;
    let ok = match (schema_ty, v) {
        (okm_core::field::FieldType::U8, Value::U8(_))
        | (okm_core::field::FieldType::U16, Value::U16(_))
        | (okm_core::field::FieldType::U32, Value::U32(_))
        | (okm_core::field::FieldType::U64, Value::U64(_))
        | (okm_core::field::FieldType::FixedBytes, Value::Bytes(_))
        | (okm_core::field::FieldType::Str, Value::Str(_))
        // Wrapper kinds carry their parameters in the schema (`Quant<P>`,
        // `Offset<B>`) and the codec applies them, so the value tree holds the
        // LOGICAL value — the same shape a decoded row has. A storage-form
        // contract would make `Quant` indistinguishable from a plain integer.
        | (okm_core::field::FieldType::Quant(_), Value::F64(_))
        | (okm_core::field::FieldType::Offset(_), Value::I64(_))
        | (okm_core::field::FieldType::VarInt, Value::U64(_))
        // ADR-0032: the id, not the label. An id is BYTES of the field's declared
        // width (`Vocab<N>`: one byte to sixteen), so `Bytes` of exactly that width
        // is the general form and `U8` the one-byte convenience. Demanding `U8`
        // alone made every `Vocab<u16>`-and-wider field unwritable through this
        // path while the encoder below already accepted its id bytes.
        | (okm_core::field::FieldType::Vocab(_), Value::U8(_)) => true,
        (okm_core::field::FieldType::Vocab(_), Value::Bytes(b)) if b.len() == field.width => true,
        _ => false,
    };
    if ok {
        Ok(v)
    } else {
        Err(CodecError::TypeMismatch {
            field: name.to_string(),
            expected: schema_predicate_vocab_name(schema_ty),
        })
    }
}

fn schema_predicate_vocab_name(ty: okm_core::field::FieldType) -> &'static str {
    match ty {
        okm_core::field::FieldType::U8 => "U8",
        okm_core::field::FieldType::U16 => "U16",
        okm_core::field::FieldType::U32 => "U32",
        okm_core::field::FieldType::U64 => "U64",
        okm_core::field::FieldType::FixedBytes => "bytes",
        okm_core::field::FieldType::Uuid4 | okm_core::field::FieldType::Uuid7 => "bytes (uuid)",
        okm_core::field::FieldType::Str => "string",
        okm_core::field::FieldType::Bytes => "bytes",
        okm_core::field::FieldType::VarInt => "U64 (VarInt storage form)",
        okm_core::field::FieldType::Quant(_) => "U64 (fixed-point storage form)",
        okm_core::field::FieldType::Vocab(_) => {
            "the vocab id (U8, or id bytes of the declared width)"
        }
        okm_core::field::FieldType::Offset(_) => "U64 (biased storage form)",
        okm_core::field::FieldType::Vector(_) => "bytes (flat LE vector)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_pair_rejects_mismatches() {
        let mut m = Dict::new();
        m.insert("a".into(), Value::U32(1));
        assert!(field_pair(&field("a", okm_core::field::FieldType::U32, 4), &m).is_ok());
        assert!(matches!(
            field_pair(&field("a", okm_core::field::FieldType::U64, 8), &m),
            Err(CodecError::TypeMismatch { .. })
        ));
        assert!(matches!(
            field_pair(&field("b", okm_core::field::FieldType::U32, 4), &m),
            Err(CodecError::MissingField(_))
        ));
    }

    fn field(
        name: &str,
        ty: okm_core::field::FieldType,
        width: usize,
    ) -> okm_core::schema::FieldSchema {
        okm_core::schema::FieldSchema {
            name: name.to_string(),
            ty,
            offset: 0,
            width,
            tag: None,
            default: None,
            expect_len: None,
        }
    }

    /// A `Vocab<u16>` id is two bytes: the pair rule had to admit them, or a
    /// categorical field wider than a byte could not be written at all.
    #[test]
    fn a_wide_vocab_id_is_admitted_by_its_width() {
        let mut m = Dict::new();
        m.insert("v".into(), Value::Bytes(vec![0, 7]));
        let vocab = field("v", okm_core::field::FieldType::Vocab(0x21), 2);
        assert!(field_pair(&vocab, &m).is_ok());
        m.insert("v".into(), Value::Bytes(vec![0, 7, 0]));
        assert!(matches!(
            field_pair(&vocab, &m),
            Err(CodecError::TypeMismatch { .. })
        ));
    }
}
