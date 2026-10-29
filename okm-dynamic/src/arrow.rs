//! Schema-driven columns: `(key bytes, decoded payload)` → `RecordBatch`.
//!
//! The typed bridge (`okm_core::bridge::arrow_bridge`) reads a `Collection`, so it
//! is bound to the declaration at compile time. A drainer has no `Collection` and
//! no row type — it has a segment file and a `CollectionSchema` sidecar — so this
//! is the other half of ADR-0045 §9: the same columns built from the SAME
//! declaration.
//!
//! Two rules keep the two from drifting:
//!
//! * the Arrow type is `okm_core::arrow_type` and nothing else (that function is
//!   public precisely so this module does not restate `FieldType → DataType`; a
//!   restated mapping is how one table becomes two Arrow schemas);
//! * a `Vocab` column carries LABELS, resolved through the caller's vocabulary
//!   (the log replays the id faces) — an id that has no label is NULL, never an
//!   invented name.

use crate::{CodecError, Value, ValueMap, decode_key};
use arrow::array::{
    ArrayRef, BinaryArray, Float64Array, Int64Array, StringArray, UInt8Array, UInt16Array,
    UInt32Array, UInt64Array,
};
use arrow::datatypes::{Field as ArrowField, Schema};
use arrow::record_batch::RecordBatch;
use okm_core::FieldType;
use okm_core::arrow_backend::arrow_type;
use okm_core::schema::{CollectionSchema, FieldSchema};
use std::sync::Arc;

/// `id slot, id bytes → label` for a `Vocab<N>` field. The drainer answers from
/// the vocabulary entries it replayed out of the log.
pub type Labels<'a> = &'a dyn Fn(u16, &[u8]) -> Option<String>;

/// The payload columns a table declares, in DECLARATION order.
///
/// `CollectionSchema` splits hot from cold and a row may interleave them
/// (`note: String` before `level: u32`), so the order is reconstructed: a cold
/// field's TLV tag is its declaration index and hot fields fill what is left in
/// their offset order. Both were assigned in declaration order, which is what
/// makes this exact rather than a guess — and a tag that is out of range or used
/// twice is a refusal, because then the declaration and the tags disagree.
pub fn ordered_fields(schema: &CollectionSchema) -> Result<Vec<&FieldSchema>, CodecError> {
    let n = schema.hot_fields.len() + schema.cold_fields.len();
    let mut slots: Vec<Option<&FieldSchema>> = vec![None; n];
    for f in &schema.cold_fields {
        let Some(tag) = f.tag else {
            return Err(CodecError::ColdFieldWithoutTag(f.name.clone()));
        };
        let i = usize::from(tag);
        if i >= n || slots[i].is_some() {
            return Err(CodecError::Column {
                what: format!(
                    "field `{}`: TLV tag {tag} is outside the declaration or used twice",
                    f.name
                ),
            });
        }
        slots[i] = Some(f);
    }
    let mut hot = schema.hot_fields.iter();
    for slot in slots.iter_mut() {
        if slot.is_none() {
            *slot = hot.next();
        }
    }
    if hot.next().is_some() {
        return Err(CodecError::Column {
            what: "more hot fields than the targets leave room for".into(),
        });
    }
    slots
        .into_iter()
        .map(|s| {
            s.ok_or_else(|| CodecError::Column {
                what: "a gap in the declaration order".into(),
            })
        })
        .collect()
}

/// Key columns first (non-nullable), then the payload half in DECLARATION order
/// (nullable: a document may have no row, and a `Vocab` id may be unlabelled).
/// The payload order must be `ordered_fields`' — arrow checks the batches built
/// against it, so a schema that says hot-then-cold while the arrays say
/// declaration order is a mismatched batch, not a subtle drift.
pub fn schema_of(schema: &CollectionSchema) -> Result<Schema, CodecError> {
    let declared = ordered_fields(schema)?;
    let mut fields = Vec::with_capacity(schema.key_fields.len() + declared.len());
    for f in &schema.key_fields {
        fields.push(ArrowField::new(f.name.clone(), arrow_type(f.ty), false));
    }
    for f in declared {
        fields.push(ArrowField::new(f.name.clone(), arrow_type(f.ty), true));
    }
    Ok(Schema::new(fields))
}

/// Build one batch: `keys[i]` is the encoded key, `rows[i]` the decoded payload
/// (`None` for a segment-only document, whole payload half NULL).
pub fn record_batch(
    schema: &CollectionSchema,
    keys: &[Vec<u8>],
    rows: &[Option<ValueMap>],
    labels: Option<Labels<'_>>,
) -> Result<RecordBatch, CodecError> {
    if keys.len() != rows.len() {
        return Err(CodecError::Column {
            what: format!("{} keys against {} rows", keys.len(), rows.len()),
        });
    }
    let declared = ordered_fields(schema)?;
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.key_fields.len() + declared.len());

    // Key half: decoded once per document, so a missing key field is a decode
    // error rather than a NULL — an identity has no hole.
    let key_maps = keys
        .iter()
        .map(|k| decode_key(schema, k))
        .collect::<Result<Vec<ValueMap>, CodecError>>()?;
    for f in &schema.key_fields {
        let vals: Vec<Option<&Value>> = key_maps
            .iter()
            .map(|m| {
                m.get(&f.name)
                    .map(Some)
                    .ok_or_else(|| CodecError::MissingField(f.name.clone()))
            })
            .collect::<Result<_, _>>()?;
        arrays.push(column(f, &vals, labels)?);
    }

    for f in declared {
        let vals: Vec<Option<&Value>> = rows
            .iter()
            .map(|r| r.as_ref().and_then(|m| m.get(&f.name)))
            .collect();
        arrays.push(column(f, &vals, labels)?);
    }

    RecordBatch::try_new(Arc::new(schema_of(schema)?), arrays).map_err(|e| CodecError::Column {
        what: format!("arrow refused the batch: {e}"),
    })
}

/// Gather one column. A NULL cell and a cell of the WRONG kind are different
/// things: the first is what a nullable column means, the second is a caller
/// writing a value the declaration never promised, and it is an error rather
/// than another NULL.
fn gather<'a, T>(
    f: &FieldSchema,
    vals: &[Option<&'a Value>],
    get: impl Fn(&'a Value) -> Option<T>,
) -> Result<Vec<Option<T>>, CodecError> {
    vals.iter()
        .map(|v| match v {
            None => Ok(None),
            Some(v) => get(v).map(Some).ok_or_else(|| CodecError::TypeMismatch {
                field: f.name.clone(),
                expected: "a value of the field's declared kind",
            }),
        })
        .collect()
}

/// One column from one declared field. Dispatch is on the DECLARED kind, the same
/// way the codec's is: `Str` and `Vocab` are both Utf8 but only the second holds
/// ids to resolve.
fn column(
    f: &FieldSchema,
    vals: &[Option<&Value>],
    labels: Option<Labels<'_>>,
) -> Result<ArrayRef, CodecError> {
    Ok(match f.ty {
        FieldType::U8 => Arc::new(UInt8Array::from_iter(gather(f, vals, |v| match v {
            Value::U8(x) => Some(*x),
            _ => None,
        })?)),
        FieldType::U16 => Arc::new(UInt16Array::from_iter(gather(f, vals, |v| match v {
            Value::U16(x) => Some(*x),
            _ => None,
        })?)),
        FieldType::U32 => Arc::new(UInt32Array::from_iter(gather(f, vals, |v| match v {
            Value::U32(x) => Some(*x),
            _ => None,
        })?)),
        // VarInt's logical value IS its integer; the frame coding lives in the
        // codec, not in the export.
        FieldType::U64 | FieldType::VarInt => {
            Arc::new(UInt64Array::from_iter(gather(f, vals, |v| match v {
                Value::U64(x) => Some(*x),
                _ => None,
            })?))
        }
        FieldType::Quant(_) => Arc::new(Float64Array::from_iter(gather(f, vals, |v| match v {
            Value::F64(x) => Some(*x),
            _ => None,
        })?)),
        FieldType::Offset(_) => Arc::new(Int64Array::from_iter(gather(f, vals, |v| match v {
            Value::I64(x) => Some(*x),
            _ => None,
        })?)),
        FieldType::Str => Arc::new(StringArray::from_iter(gather(f, vals, |v| match v {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        })?)),
        FieldType::Vocab(id_slot) => {
            let ids = gather(f, vals, |v| match v {
                Value::Bytes(b) => Some(b.as_slice()),
                Value::U8(x) => Some(std::slice::from_ref(x)),
                _ => None,
            })?;
            let named: Vec<Option<String>> = ids
                .iter()
                .map(|id| match (id, labels) {
                    (Some(id), Some(resolve)) => resolve(id_slot, id),
                    _ => None,
                })
                .collect();
            Arc::new(StringArray::from_iter(named.iter().map(|s| s.as_deref())))
        }
        FieldType::FixedBytes | FieldType::Bytes | FieldType::Vector { .. } => {
            Arc::new(BinaryArray::from_iter(gather(f, vals, |v| match v {
                Value::Bytes(b) => Some(b.as_slice()),
                _ => None,
            })?))
        }
    })
}
