//! Decoding walks: bytes + schema → value tree. Pure pointer slicing on
//! the key; payload walks the header, the hot segment by static offsets,
//! then cold TLV frames by tag.

use crate::{CodecError, TableSchema, Value, ValueMap};
use okm_core::field::FieldType;
use std::collections::BTreeMap;

/// Decode the key bytes into the value map (fields at their static
/// offsets; nested segments would recurse — v1 keys are flat).
pub fn decode_key(schema: &TableSchema, bytes: &[u8]) -> Result<ValueMap, CodecError> {
    if bytes.len() < schema.key_len {
        return Err(CodecError::Truncated {
            field: "<key>".into(),
            needed: schema.key_len,
            got: bytes.len(),
        });
    }
    let mut out = BTreeMap::new();
    for f in &schema.key_fields {
        let raw = &bytes[f.offset..f.offset + f.width];
        out.insert(f.name.clone(), decode_fixed(f.name.clone(), f.ty, f.width, raw)?);
    }
    Ok(out)
}

/// Decode the payload bytes (the two-segment region, no key bytes):
/// header version checked against the schema, hot fields sliced at
/// static offsets, cold TLV frames matched by tag. Unknown tags are
/// skipped — forward-compatible reading of fields the schema does not
/// know is the TLV contract (ADR-0004).
pub fn decode_payload(schema: &TableSchema, bytes: &[u8]) -> Result<ValueMap, CodecError> {
    let hdr = schema.payload_header_len;
    if bytes.len() < hdr {
        return Err(CodecError::Truncated {
            field: "<payload header>".into(),
            needed: hdr,
            got: bytes.len(),
        });
    }
    let version = bytes[0];
    if version > schema.layout_version {
        return Err(CodecError::VersionMismatch {
            schema: schema.layout_version,
            found: version,
        });
    }
    let hot_len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    let hot = bytes
        .get(hdr..hdr + hot_len)
        .ok_or_else(|| CodecError::Truncated {
            field: "<hot segment>".into(),
            needed: hot_len,
            got: bytes.len().saturating_sub(hdr),
        })?;

    let mut out = BTreeMap::new();
    for f in &schema.hot_fields {
        let raw = hot.get(f.offset..f.offset + f.width).ok_or_else(|| {
            CodecError::Truncated {
                field: f.name.clone(),
                needed: f.offset + f.width,
                got: hot.len(),
            }
        })?;
        out.insert(f.name.clone(), decode_fixed(f.name.clone(), f.ty, f.width, raw)?);
    }

    // Cold TLV frames: tag u8 + len u32 BE + value, until the tail.
    let by_tag: BTreeMap<u8, &okm_core::schema::FieldSchema> = schema
        .cold_fields
        .iter()
        .filter_map(|f| f.tag.map(|t| (t, f)))
        .collect();
    let mut pos = hdr + hot_len;
    while pos < bytes.len() {
        let tag = *bytes.get(pos).ok_or_else(|| CodecError::Truncated {
            field: "<cold tag>".into(),
            needed: 1,
            got: bytes.len() - pos,
        })?;
        pos += 1;
        let len = u32::from_be_bytes(
            bytes
                .get(pos..pos + 4)
                .ok_or_else(|| CodecError::Truncated {
                    field: "<cold len>".into(),
                    needed: 4,
                    got: bytes.len() - pos,
                })?
                .try_into()
                .expect("4-byte slice"),
        ) as usize;
        pos += 4;
        let value = bytes.get(pos..pos + len).ok_or_else(|| CodecError::Truncated {
            field: "<cold value>".into(),
            needed: len,
            got: bytes.len().saturating_sub(pos),
        })?;
        pos += len;
        match by_tag.get(&tag) {
            // Known field: decode per its kind.
            Some(f) => {
                let v = match f.ty {
                    FieldType::Str => Value::Str(
                        std::str::from_utf8(value)
                            .map_err(|_| CodecError::InvalidUtf8(f.name.clone()))?
                            .to_string(),
                    ),
                    FieldType::VarInt | FieldType::Quant(_) | FieldType::Offset(_) => {
                        if value.len() != 8 {
                            return Err(CodecError::Truncated {
                                field: f.name.clone(),
                                needed: 8,
                                got: value.len(),
                            });
                        }
                        let x = u64::from_be_bytes(value.try_into().unwrap());
                        Value::U64(x)
                    }
                    _ => {
                        return Err(CodecError::TypeMismatch {
                            field: f.name.clone(),
                            expected: "cold-capable kind (Str / wrapper)",
                        })
                    }
                };
                out.insert(f.name.clone(), v);
            }
            // Unknown tag: skip the frame (forward compatibility, ADR-0004).
            None => {
                let _ = CodecError::UnknownTag(tag);
            }
        }
    }
    Ok(out)
}

fn decode_fixed(
    name: String,
    ty: FieldType,
    width: usize,
    raw: &[u8],
) -> Result<Value, CodecError> {
    let need = |got: usize| CodecError::Truncated {
        field: name.clone(),
        needed: width,
        got,
    };
    Ok(match ty {
        FieldType::U8 => Value::U8(*raw.first().ok_or_else(|| need(raw.len()))?),
        FieldType::U16 => Value::U16(u16::from_be_bytes(
            raw.try_into().map_err(|_| need(raw.len()))?,
        )),
        FieldType::U32 => Value::U32(u32::from_be_bytes(
            raw.try_into().map_err(|_| need(raw.len()))?,
        )),
        FieldType::U64 => Value::U64(u64::from_be_bytes(
            raw.try_into().map_err(|_| need(raw.len()))?,
        )),
        FieldType::FixedBytes => Value::Bytes(raw.to_vec()),
        // Str/Bytes are cold-only; hot segment never carries them (width 0).
        FieldType::Str | FieldType::Bytes | FieldType::VarInt | FieldType::Quant(_)
        | FieldType::Enum | FieldType::Offset(_) => {
            return Err(CodecError::TypeMismatch {
                field: name,
                expected: "hot-capable fixed-width kind",
            })
        }
    })
}
