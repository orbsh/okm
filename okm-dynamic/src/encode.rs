//! Encoding walks: schema fields in declaration order, values from the
//! map. Key encoding is the flat concatenation of BE fields; payload is
//! `[version u8][hot_len u16 BE][hot][cold TLV]`.

use crate::{field_pair, CodecError, CollectionSchema, Value, ValueMap};

/// Encode the key from the value map: fields in schema order, BE widths.
/// Returns exactly `schema.key_len` bytes (nested segments recurse).
pub fn encode_key(schema: &CollectionSchema, values: &ValueMap) -> Result<Vec<u8>, CodecError> {
    let mut buf = Vec::with_capacity(schema.key_len);
    for f in &schema.key_fields {
        encode_field(f, values, &mut buf)?;
    }
    debug_assert_eq!(buf.len(), schema.key_len, "key width composition must hold");
    Ok(buf)
}

/// Encode the payload: `[version u8][hot_len u16 BE][hot][cold TLV]`.
/// Cold frames are written in schema declaration order (the map order is
/// irrelevant); every declared cold field MUST be present — dynamic
/// payloads are fully materialized (no partial documents).
pub fn encode_payload(schema: &CollectionSchema, values: &ValueMap) -> Result<Vec<u8>, CodecError> {
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
    let mut buf = Vec::with_capacity(schema.payload_header_len + schema.hot_width + 16);
    buf.push(schema.layout_version);
    buf.extend_from_slice(&(hot.len() as u16).to_be_bytes());
    buf.extend_from_slice(&hot);
    for f in &schema.cold_fields {
        let v = field_pair(&f.name, f.ty, values)?;
        let Some(tag) = f.tag else {
            return Err(CodecError::ColdFieldWithoutTag(f.name.clone()));
        };
        buf.push(tag);
        match v {
            Value::Str(s) => {
                okm_core::put_len(&mut buf, s.len());
                buf.extend_from_slice(s.as_bytes());
            }
            other => {
                let bytes = cold_frame_bytes(f, other)?;
                okm_core::put_len(&mut buf, bytes.len());
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
    values: &ValueMap,
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
            let v = field_pair(&f.name, f.ty, values)?;
            match v {
                Value::Str(s) => buf.extend_from_slice(s.as_bytes()),
                _ => {
                    return Err(CodecError::TypeMismatch {
                        field: f.name.clone(),
                        expected: "variable-width (Str)",
                    })
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
    f: &okm_core::schema::FieldSchema,
    values: &ValueMap,
    buf: &mut Vec<u8>,
) -> Result<(), CodecError> {
    let v = field_pair(&f.name, f.ty, values)?;
    debug_assert_eq!(buf.len(), f.offset, "static placement must be contiguous");
    // Wrapper kinds carry their parameters in the SCHEMA — `Quant<P>` its
    // precision, `Offset<B>` its base — so the codec does the arithmetic. The
    // other contract, "hand me the storage form", pushes the precision and the
    // base back into every caller's hands and is exactly the drift the schema
    // export exists to prevent (and what made these kinds write an 8-byte image
    // no typed reader could consume).
    match f.ty {
        okm_core::FieldType::Quant(p) => {
            let Value::F64(x) = v else {
                return Err(type_err(&f.name, "f64 (Quant's logical value)"));
            };
            buf.extend_from_slice(&okm_core::wire_to_be_bytes(okm_core::quantize(*x, p)));
            return Ok(());
        }
        okm_core::FieldType::Offset(base) => {
            let Value::I64(x) = v else {
                return Err(type_err(&f.name, "i64 (Offset's logical value)"));
            };
            buf.extend_from_slice(&offset_bytes(*x, base, &f.name)?);
            return Ok(());
        }
        // Variable width, so cold by construction: a hot walk that reaches a
        // VarInt field means the schema disagrees with the declaration.
        okm_core::FieldType::VarInt => {
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
            })
        }
        // Dynamic-segment parity kinds: typed-schema fields cannot declare
        // them yet (schema kinds are fixed-width), so in the static region
        // they are type errors. They become valid once a schema kind lands.
        Value::I64(_) | Value::F64(_) | Value::Bool(_) | Value::Null | Value::Obj(_) | Value::Array(_) => {
            return Err(CodecError::TypeMismatch {
                field: f.name.clone(),
                expected: "fixed-width numeric/bytes (signed/float/bool/null are dynamic-segment kinds)",
            })
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
fn cold_frame_bytes(
    f: &okm_core::schema::FieldSchema,
    v: &Value,
) -> Result<Vec<u8>, CodecError> {
    Ok(match f.ty {
        okm_core::FieldType::VarInt => {
            let Value::U64(x) = v else {
                return Err(type_err(&f.name, "u64 (VarInt's logical value)"));
            };
            okm_core::VarInt::<u64>(*x).encode()
        }
        okm_core::FieldType::Quant(p) => {
            let Value::F64(x) = v else {
                return Err(type_err(&f.name, "f64 (Quant's logical value)"));
            };
            okm_core::wire_to_be_bytes(okm_core::quantize(*x, p))
        }
        okm_core::FieldType::Offset(base) => {
            let Value::I64(x) = v else {
                return Err(type_err(&f.name, "i64 (Offset's logical value)"));
            };
            offset_bytes(*x, base, &f.name)?
        }
        // ADR-0032: the frame is the id; a label is what a reader resolves, not
        // something a writer stores. Both spellings of an id are accepted so a
        // decoded row (`Bytes`) re-encodes without a translation step.
        okm_core::FieldType::Vocab(_) => match v {
            Value::U8(x) => vec![*x],
            Value::Bytes(b) if b.len() == f.width => b.clone(),
            _ => return Err(type_err(&f.name, "the vocabulary id (u8 / width bytes)")),
        },
        okm_core::FieldType::Bytes => match v {
            Value::Bytes(b) => b.clone(),
            Value::Str(s) => s.as_bytes().to_vec(),
            _ => return Err(type_err(&f.name, "raw bytes")),
        },
        okm_core::FieldType::FixedBytes => match v {
            Value::Bytes(b) if b.len() == f.width => b.clone(),
            _ => return Err(type_err(&f.name, "bytes of the declared width")),
        },
        okm_core::FieldType::U8 => match v {
            Value::U8(x) => vec![*x],
            _ => return Err(type_err(&f.name, "u8")),
        },
        okm_core::FieldType::U16 => match v {
            Value::U16(x) => x.to_be_bytes().to_vec(),
            _ => return Err(type_err(&f.name, "u16")),
        },
        okm_core::FieldType::U32 => match v {
            Value::U32(x) => x.to_be_bytes().to_vec(),
            _ => return Err(type_err(&f.name, "u32")),
        },
        okm_core::FieldType::U64 => match v {
            Value::U64(x) => x.to_be_bytes().to_vec(),
            _ => return Err(type_err(&f.name, "u64")),
        },
        // Str is framed by `encode_payload` (no length prefix inside the frame
        // beyond the TLV's own) and Vector frames are not built here yet.
        okm_core::FieldType::Str => return Err(type_err(&f.name, "Str (framed as UTF-8)")),
        okm_core::FieldType::Vector { .. } => {
            return Err(type_err(
                &f.name,
                "a Vector frame — the dynamic codec does not build element frames yet",
            ));
        }
    })
}

/// `value − base` as a 4-byte BE u32, WITHOUT the panic in
/// [`okm_core::offset_encode`]: this codec takes caller-supplied values, so an
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
