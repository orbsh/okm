//! okm-dynamic — schema-driven codec for embedded-language Actors
//! (ADR-0010 PLAN "dynamic codec").
//!
//! Host-independent core: given a [`TableSchema`] (exported from Rust by
//! `okm-core::schema::TableSchema::of`) and a plain value tree, encode
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
//! Value tree representation is host-agnostic (`Value` enum); binding
//! crates convert to/from their native types (dict, hashmap, ...).
//!
//! Encoding rules (mirroring the derive, byte-for-byte):
//! - key fields: BE fixed-width at their static offsets, concatenated;
//! - payload: `[version u8][hot_len u16 BE][hot fields][cold TLV]`,
//!   cold frames `[tag u8][len u32 BE][value]` in declaration order;

mod decode;
mod encode;
mod graph;
mod index;
mod collection;
mod reduce;

pub use okm_core::schema::{FieldSchema, TableSchema};
pub use okm_core::field::FieldType;

// NOTE (v1 scope): nested key segments are NOT supported by the dynamic
// codec — `FieldType::Segment` does not exist (the key-composition
// primitive was rejected; see PLAN). Key schemas here are flat primitive
// fields. Should segments return, the schema export already carries the
// FieldType variant slot for them.

pub use decode::{decode_key, decode_payload};
pub use encode::{encode_key, encode_payload};
pub use index::{index_entries, scan_access_method, AccessMethod, AccessMethodKind, Admits, FuncDerive};
pub use collection::DynamicCollection;
pub use graph::{DynEdge, Graph};
pub use reduce::{scan_reduces, reduce_get, AccOp, BoundReduce, ReduceSpec};

use std::collections::BTreeMap;

/// A schema-driven value: the dynamic side's currency. Field order is
/// irrelevant (placement comes from the schema); unknown fields are
/// rejected — a value tree that does not match the schema is a caller
/// bug, not silently dropped bytes.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    /// Signed integer (dynamic-segment parity; typed fields may declare
    /// signed kinds when schema support lands).
    I64(i64),
    /// IEEE-754 double (dynamic-segment parity).
    F64(f64),
    /// Boolean (dynamic-segment parity).
    Bool(bool),
    /// Absence (dynamic-segment parity; encodes as a zero-length frame).
    Null,
    /// Fixed-width byte string (`[u8; N]` key fields, `FixedBytes`).
    Bytes(Vec<u8>),
    /// UTF-8 string (cold TLV payload fields).
    Str(String),
}

/// Field name → value. BTreeMap for deterministic iteration (cold TLV
/// frames are written in schema declaration order, not map order).
pub type ValueMap = BTreeMap<String, Value>;

/// Codec errors: schema/value mismatches, out-of-range values, truncated
/// bytes. Dynamic-side mistakes are normal input — errors, never panics.
#[derive(Clone, Debug, PartialEq)]
pub enum CodecError {
    /// Field present in the value tree but absent from the schema.
    UnknownField(String),
    /// Schema field missing from the value tree.
    MissingField(String),
    /// Value variant does not match the field's declared kind.
    TypeMismatch { field: String, expected: &'static str },
    /// Encoded bytes shorter than the schema requires.
    Truncated { field: String, needed: usize, got: usize },
    /// Payload header version newer than the schema's.
    VersionMismatch { schema: u8, found: u8 },
    /// Cold TLV frame with a tag outside the schema's cold field set.
    UnknownTag(u8),
    /// Invalid UTF-8 in a Str field.
    InvalidUtf8(String),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::UnknownField(n) => write!(f, "unknown field `{n}` (not in schema)"),
            CodecError::MissingField(n) => write!(f, "missing field `{n}`"),
            CodecError::TypeMismatch { field, expected } => {
                write!(f, "field `{field}`: expected {expected}")
            }
            CodecError::Truncated { field, needed, got } => {
                write!(f, "field `{field}` truncated: need {needed} bytes, got {got}")
            }
            CodecError::VersionMismatch { schema, found } => {
                write!(f, "payload version {found} newer than schema {schema}")
            }
            CodecError::UnknownTag(t) => write!(f, "unknown cold TLV tag {t}"),
            CodecError::InvalidUtf8(n) => write!(f, "field `{n}`: invalid UTF-8"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Look up a field's value, rejecting unknown/missing names up front —
/// both sides of every encode walk this gate, so byte layout and error
/// reporting stay in one place.
pub(crate) fn field_pair<'a>(
    name: &str,
    schema_ty: okm_core::field::FieldType,
    map: &'a ValueMap,
) -> Result<&'a Value, CodecError> {
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
        // VarInt/Quant/Enum/Offset are payload wrappers with host-specific
        // dynamic encodings in v1 scope: encode their STORAGE form via
        // the matching integer/bytes variant (documented per-field in the
        // binding layer). Here: Quant/Offset → U64 storage, Enum → U8,
        // VarInt → U64.
        | (okm_core::field::FieldType::Quant(_), Value::U64(_))
        | (okm_core::field::FieldType::Offset(_), Value::U64(_))
        | (okm_core::field::FieldType::Enum, Value::U8(_))
        | (okm_core::field::FieldType::VarInt, Value::U64(_)) => true,
        _ => false,
    };
    if ok {
        Ok(v)
    } else {
        Err(CodecError::TypeMismatch {
            field: name.to_string(),
            expected: schema_kind_name(schema_ty),
        })
    }
}

fn schema_kind_name(ty: okm_core::field::FieldType) -> &'static str {
    match ty {
        okm_core::field::FieldType::U8 => "U8",
        okm_core::field::FieldType::U16 => "U16",
        okm_core::field::FieldType::U32 => "U32",
        okm_core::field::FieldType::U64 => "U64",
        okm_core::field::FieldType::FixedBytes => "bytes",
        okm_core::field::FieldType::Str => "string",
        okm_core::field::FieldType::Bytes => "bytes",
        okm_core::field::FieldType::VarInt => "U64 (VarInt storage form)",
        okm_core::field::FieldType::Quant(_) => "U64 (fixed-point storage form)",
        okm_core::field::FieldType::Enum => "U8 (tag)",
        okm_core::field::FieldType::Offset(_) => "U64 (biased storage form)",
        okm_core::field::FieldType::Vector { .. } => "bytes (flat LE vector)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_pair_rejects_mismatches() {
        let mut m = ValueMap::new();
        m.insert("a".into(), Value::U32(1));
        assert!(field_pair("a", okm_core::field::FieldType::U32, &m).is_ok());
        assert!(matches!(
            field_pair("a", okm_core::field::FieldType::U64, &m),
            Err(CodecError::TypeMismatch { .. })
        ));
        assert!(matches!(
            field_pair("b", okm_core::field::FieldType::U32, &m),
            Err(CodecError::MissingField(_))
        ));
    }
}
