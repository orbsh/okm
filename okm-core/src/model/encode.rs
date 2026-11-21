//! Encoding walks: schema fields in declaration order, values from the
//! map. Key encoding is the flat concatenation of BE fields; payload is
//! `[hot][cold TLV]` — no header at all: the hot width is the declaration's
//! (`HOT_WIDTH`), and the payload's generation is the SEGMENT header's statement
//! (ADR-0042 as amended), so a row carries neither a length nor a version.

use crate::model::schema::CollectionSchema;
use okm_value::{CodecError, Dict, Variant as Value};

/// Encode the key from the value map: fields in schema order, BE widths.
/// Returns exactly `schema.key_len` bytes (nested segments recurse).
pub fn encode_key(schema: &CollectionSchema, values: &Dict) -> Result<Vec<u8>, CodecError> {
    let mut buf = Vec::with_capacity(schema.key_len);
    for f in &schema.key_fields {
        encode_field(f, values, &mut buf)?;
    }
    debug_assert_eq!(buf.len(), schema.key_len, "key width composition must hold");
    Ok(buf)
}

/// Encode the payload: `[hot][cold TLV]`.
/// Cold frames are written in schema declaration order (the map order is
/// irrelevant); every declared cold field MUST be present — dynamic
/// payloads are fully materialized (no partial documents).
pub fn encode_payload(schema: &CollectionSchema, values: &Dict) -> Result<Vec<u8>, CodecError> {
    let mut hot = Vec::with_capacity(schema.hot_width);
    for f in &schema.hot_fields {
        encode_field(f, values, &mut hot)?;
    }
    if hot.len() != schema.hot_width {
        return Err(CodecError::Truncated {
            field: "<hot segment>".into(),
            needed: schema.hot_width,
            got: hot.len(),
        });
    }
    // No header: hot width is the declaration's, so the payload starts at field 0.
    let mut buf = Vec::with_capacity(schema.hot_width + 16);
    buf.extend_from_slice(&hot);
    for f in &schema.cold_fields {
        let v = field_pair(f, values)?;
        let Some(tag) = f.tag else {
            return Err(CodecError::ColdFieldWithoutTag(f.name.clone()));
        };
        buf.push(tag);
        match v {
            Value::Str(s) => {
                crate::put_len(&mut buf, s.len());
                buf.extend_from_slice(s.as_bytes());
            }
            other => {
                let bytes = cold_frame_bytes(f, other)?;
                crate::put_len(&mut buf, bytes.len());
                buf.extend_from_slice(&bytes);
            }
        }
    }
    Ok(buf)
}

/// Encode the given schema fields (by name, in the caller's order) from
/// a value map — the general primitive behind key and index-prefix
/// encoding. All named fields must be fixed-width schema fields (key or
/// hot); variable-length kinds error (they cannot order as prefixes).
pub fn encode_fields(
    schema: &CollectionSchema,
    names: &[String],
    values: &Dict,
) -> Result<Vec<u8>, CodecError> {
    let mut buf = Vec::new();
    for name in names {
        let f = schema
            .key_fields
            .iter()
            .chain(schema.hot_fields.iter())
            .chain(schema.cold_fields.iter())
            .find(|f| &f.name == name)
            .ok_or_else(|| CodecError::UnknownField(name.clone()))?;
        // A cold (variable-width) field encodes as raw storage bytes in a
        // probe/index segment (the trailing-tail regime — raw UTF-8 for
        // Str, no frame); hot/key fields keep their fixed-width walk.
        if f.width == 0 {
            let v = field_pair(f, values)?;
            match v {
                Value::Str(s) => buf.extend_from_slice(s.as_bytes()),
                _ => {
                    return Err(CodecError::TypeMismatch {
                        field: f.name.clone(),
                        expected: "variable-width (Str)",
                    });
                }
            }
        } else {
            encode_field(f, values, &mut buf)?;
        }
    }
    Ok(buf)
}

/// Encode one field (key or hot — fixed-width, static placement). Cold
/// fields go through `encode_payload`'s TLV path.
fn encode_field(
    f: &crate::schema::FieldSchema,
    values: &Dict,
    buf: &mut Vec<u8>,
) -> Result<(), CodecError> {
    let v = field_pair(f, values)?;
    debug_assert_eq!(buf.len(), f.offset, "static placement must be contiguous");
    // Wrapper kinds carry their parameters in the SCHEMA — `Quant<P>` its
    // precision, `Offset<B>` its base — so the codec does the arithmetic. The
    // other contract, "hand me the storage form", pushes the precision and the
    // base back into every caller's hands and is exactly the drift the schema
    // export exists to prevent (and what made these kinds write an 8-byte image
    // no typed reader could consume).
    match f.ty {
        crate::FieldType::Quant(p) => {
            let Value::F64(x) = v else {
                return Err(type_err(&f.name, "f64 (Quant's logical value)"));
            };
            buf.extend_from_slice(&crate::wire_to_be_bytes(crate::quantize(*x, p)));
            return Ok(());
        }
        crate::FieldType::Offset(base) => {
            let Value::I64(x) = v else {
                return Err(type_err(&f.name, "i64 (Offset's logical value)"));
            };
            buf.extend_from_slice(&offset_bytes(*x, base, &f.name)?);
            return Ok(());
        }
        // Variable width, so cold by construction: a hot walk that reaches a
        // VarInt field means the schema disagrees with the declaration.
        crate::FieldType::VarInt => {
            return Err(type_err(&f.name, "cold TLV (VarInt has no static width)"));
        }
        _ => {}
    }
    match v {
        Value::U8(x) => buf.push(*x),
        Value::U16(x) => buf.extend_from_slice(&x.to_be_bytes()),
        Value::U32(x) => buf.extend_from_slice(&x.to_be_bytes()),
        Value::U64(x) => buf.extend_from_slice(&x.to_be_bytes()),
        Value::Bytes(b) => {
            if b.len() != f.width {
                return Err(CodecError::Truncated {
                    field: f.name.clone(),
                    needed: f.width,
                    got: b.len(),
                });
            }
            buf.extend_from_slice(b);
        }
        Value::Str(_) => {
            return Err(CodecError::TypeMismatch {
                field: f.name.clone(),
                expected: "fixed-width (Str is a cold TLV kind)",
            });
        }
        // Dynamic-segment parity kinds: typed-schema fields cannot declare
        // them yet (schema kinds are fixed-width), so in the static region
        // they are type errors. They become valid once a schema kind lands.
        Value::I64(_)
        | Value::F64(_)
        | Value::Bool(_)
        | Value::Null
        | Value::Map(_)
        | Value::Array(_) => {
            return Err(CodecError::TypeMismatch {
                field: f.name.clone(),
                expected: "fixed-width numeric/bytes (signed/float/bool/null are dynamic-segment kinds)",
            });
        }
    }
    Ok(())
}

/// Cold TLV frame bytes for a kind other than `Str`.
///
/// Dispatch is on the FIELD KIND, never on the value alone: `VarInt` is a
/// prefix-monotonic varint (its frame length is the encoding's), `Quant` a
/// fixed-point i64 wire, `Offset` a 4-byte biased u32, `Vocab` one id byte.
/// Writing all of them as an 8-byte BE image — which is what a value-only
/// dispatch did — produces frames the typed reader cannot consume: a 3-byte
/// varint frame overruns, a 4-byte offset frame truncates, and a quantised
/// value comes back as an integer.
fn cold_frame_bytes(f: &crate::schema::FieldSchema, v: &Value) -> Result<Vec<u8>, CodecError> {
    Ok(match f.ty {
        crate::FieldType::VarInt => {
            let Value::U64(x) = v else {
                return Err(type_err(&f.name, "u64 (VarInt's logical value)"));
            };
            crate::VarInt::<u64>(*x).encode()
        }
        crate::FieldType::Quant(p) => {
            let Value::F64(x) = v else {
                return Err(type_err(&f.name, "f64 (Quant's logical value)"));
            };
            crate::wire_to_be_bytes(crate::quantize(*x, p))
        }
        crate::FieldType::Offset(base) => {
            let Value::I64(x) = v else {
                return Err(type_err(&f.name, "i64 (Offset's logical value)"));
            };
            offset_bytes(*x, base, &f.name)?
        }
        // ADR-0032: the frame is the id; a label is what a reader resolves, not
        // something a writer stores. Both spellings of an id are accepted so a
        // decoded row (`Bytes`) re-encodes without a translation step.
        crate::FieldType::Vocab(_) => match v {
            Value::U8(x) => vec![*x],
            Value::Bytes(b) if b.len() == f.width => b.clone(),
            _ => return Err(type_err(&f.name, "the vocabulary id (u8 / width bytes)")),
        },
        crate::FieldType::Bytes => match v {
            Value::Bytes(b) => b.clone(),
            Value::Str(s) => s.as_bytes().to_vec(),
            _ => return Err(type_err(&f.name, "raw bytes")),
        },
        crate::FieldType::FixedBytes => match v {
            Value::Bytes(b) if b.len() == f.width => b.clone(),
            _ => return Err(type_err(&f.name, "bytes of the declared width")),
        },
        // A uuid is exactly 16 bytes: the length check is the type check.
        crate::FieldType::Uuid4 | crate::FieldType::Uuid7 => match v {
            Value::Bytes(b) if b.len() == 16 => b.clone(),
            _ => return Err(type_err(&f.name, "bytes of a 16-byte uuid")),
        },
        crate::FieldType::U8 => match v {
            Value::U8(x) => vec![*x],
            _ => return Err(type_err(&f.name, "u8")),
        },
        crate::FieldType::U16 => match v {
            Value::U16(x) => x.to_be_bytes().to_vec(),
            _ => return Err(type_err(&f.name, "u16")),
        },
        crate::FieldType::U32 => match v {
            Value::U32(x) => x.to_be_bytes().to_vec(),
            _ => return Err(type_err(&f.name, "u32")),
        },
        crate::FieldType::U64 => match v {
            Value::U64(x) => x.to_be_bytes().to_vec(),
            _ => return Err(type_err(&f.name, "u64")),
        },
        // Str is framed by `encode_payload` (no length prefix inside the frame
        // beyond the TLV's own) and Vector frames are not built here yet.
        crate::FieldType::Str => return Err(type_err(&f.name, "Str (framed as UTF-8)")),
        crate::FieldType::Vector(_) => {
            return Err(type_err(
                &f.name,
                "a Vector frame — the dynamic codec does not build element frames yet",
            ));
        }
    })
}

/// `value − base` as a 4-byte BE u32, WITHOUT the panic in
/// [`crate::offset_encode`]: this codec takes caller-supplied values, so an
/// out-of-window offset is an error the caller can act on, not a crash.
fn offset_bytes(value: i64, base: i64, name: &str) -> Result<Vec<u8>, CodecError> {
    value
        .checked_sub(base)
        .filter(|d| (0..=u32::MAX as i64).contains(d))
        .map(|d| (d as u32).to_be_bytes().to_vec())
        .ok_or_else(|| {
            type_err(
                name,
                "an Offset value inside [base, base + 2^32) (u32 wire)",
            )
        })
}

fn type_err(field: &str, expected: &'static str) -> CodecError {
    CodecError::TypeMismatch {
        field: field.to_string(),
        expected,
    }
}

/// Look up a field's value, rejecting unknown/missing names up front —
/// both sides of every encode walk this gate, so byte layout and error
/// reporting stay in one place.
pub fn field_pair<'a>(
    field: &crate::schema::FieldSchema,
    map: &'a Dict,
) -> Result<&'a Value, CodecError> {
    let name = field.name.as_str();
    let schema_ty = field.ty;
    let v = map
        .get(name)
        .ok_or_else(|| CodecError::MissingField(name.to_string()))?;
    let ok = match (schema_ty, v) {
        (crate::field::FieldType::U8, Value::U8(_))
        | (crate::field::FieldType::U16, Value::U16(_))
        | (crate::field::FieldType::U32, Value::U32(_))
        | (crate::field::FieldType::U64, Value::U64(_))
        | (crate::field::FieldType::FixedBytes, Value::Bytes(_))
        | (crate::field::FieldType::Str, Value::Str(_))
        // Wrapper kinds carry their parameters in the schema (`Quant<P>`,
        // `Offset<B>`) and the codec applies them, so the value tree holds the
        // LOGICAL value — the same shape a decoded row has. A storage-form
        // contract would make `Quant` indistinguishable from a plain integer.
        | (crate::field::FieldType::Quant(_), Value::F64(_))
        | (crate::field::FieldType::Offset(_), Value::I64(_))
        | (crate::field::FieldType::VarInt, Value::U64(_))
        // ADR-0032: the id, not the label. An id is BYTES of the field's declared
        // width (`Vocab<N>`: one byte to sixteen), so `Bytes` of exactly that width
        // is the general form and `U8` the one-byte convenience. Demanding `U8`
        // alone made every `Vocab<u16>`-and-wider field unwritable through this
        // path while the encoder below already accepted its id bytes.
        | (crate::field::FieldType::Vocab(_), Value::U8(_)) => true,
        (crate::field::FieldType::Vocab(_), Value::Bytes(b)) if b.len() == field.width => true,
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

fn schema_predicate_vocab_name(ty: crate::field::FieldType) -> &'static str {
    match ty {
        crate::field::FieldType::U8 => "U8",
        crate::field::FieldType::U16 => "U16",
        crate::field::FieldType::U32 => "U32",
        crate::field::FieldType::U64 => "U64",
        crate::field::FieldType::FixedBytes => "bytes",
        crate::field::FieldType::Uuid4 | crate::field::FieldType::Uuid7 => "bytes (uuid)",
        crate::field::FieldType::Str => "string",
        crate::field::FieldType::Bytes => "bytes",
        crate::field::FieldType::VarInt => "U64 (VarInt storage form)",
        crate::field::FieldType::Quant(_) => "U64 (fixed-point storage form)",
        crate::field::FieldType::Vocab(_) => "the vocab id (U8, or id bytes of the declared width)",
        crate::field::FieldType::Offset(_) => "U64 (biased storage form)",
        crate::field::FieldType::Vector(_) => "bytes (flat LE vector)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_pair_rejects_mismatches() {
        let mut m = Dict::new();
        m.insert("a".into(), Value::U32(1));
        assert!(field_pair(&field("a", crate::field::FieldType::U32, 4), &m).is_ok());
        assert!(matches!(
            field_pair(&field("a", crate::field::FieldType::U64, 8), &m),
            Err(CodecError::TypeMismatch { .. })
        ));
        assert!(matches!(
            field_pair(&field("b", crate::field::FieldType::U32, 4), &m),
            Err(CodecError::MissingField(_))
        ));
    }

    fn field(name: &str, ty: crate::field::FieldType, width: usize) -> crate::schema::FieldSchema {
        crate::schema::FieldSchema {
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
        let vocab = field("v", crate::field::FieldType::Vocab(0x21), 2);
        assert!(field_pair(&vocab, &m).is_ok());
        m.insert("v".into(), Value::Bytes(vec![0, 7, 0]));
        assert!(matches!(
            field_pair(&vocab, &m),
            Err(CodecError::TypeMismatch { .. })
        ));
    }
}
