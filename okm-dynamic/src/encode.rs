//! Encoding walks: schema fields in declaration order, values from the
//! map. Key encoding is the flat concatenation of BE fields; payload is
//! `[version u8][hot_len u16 BE][hot][cold TLV]`.

use crate::{field_pair, CodecError, TableSchema, Value, ValueMap};

/// Encode the key from the value map: fields in schema order, BE widths.
/// Returns exactly `schema.key_len` bytes (nested segments recurse).
pub fn encode_key(schema: &TableSchema, values: &ValueMap) -> Result<Vec<u8>, CodecError> {
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
/// payloads are fully materialized (no partial rows).
pub fn encode_payload(schema: &TableSchema, values: &ValueMap) -> Result<Vec<u8>, CodecError> {
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
        let tag = f.tag.expect("cold field carries a tag");
        buf.push(tag);
        match v {
            Value::Str(s) => {
                buf.extend_from_slice(&(s.len() as u32).to_be_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            other => {
                // Variable-width wrapper kinds arrive in their storage
                // form; frame them through their integer encoding.
                let bytes = storage_be_bytes(other, &f.name)?;
                buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                buf.extend_from_slice(&bytes);
            }
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
        Value::I64(_) | Value::F64(_) | Value::Bool(_) | Value::Null => {
            return Err(CodecError::TypeMismatch {
                field: f.name.clone(),
                expected: "fixed-width numeric/bytes (signed/float/bool/null are dynamic-segment kinds)",
            })
        }
    }
    Ok(())
}

/// Variable-width wrapper kinds in storage form (VarInt LEB128, Enum tag
/// byte already covered by U8, Quant/Offset are fixed 8B/4B BE).
fn storage_be_bytes(v: &Value, name: &str) -> Result<Vec<u8>, CodecError> {
    Ok(match v {
        Value::U64(x) => x.to_be_bytes().to_vec(),
        Value::U8(x) => vec![*x],
        _ => {
            return Err(CodecError::TypeMismatch {
                field: name.to_string(),
                expected: "integer storage form",
            })
        }
    })
}
