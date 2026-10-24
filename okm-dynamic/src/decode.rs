//! Decoding walks: bytes + schema → value tree. Pure pointer slicing on
//! the key; payload walks the header, the hot segment by static offsets,
//! then cold TLV frames by tag.

use crate::{CodecError, CollectionSchema, Value, ValueMap};
use okm_core::field::FieldType;
use std::collections::BTreeMap;

/// Decode the key bytes into the value map (fields at their static
/// offsets; nested segments would recurse — v1 keys are flat).
pub fn decode_key(schema: &CollectionSchema, bytes: &[u8]) -> Result<ValueMap, CodecError> {
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

/// Replace every `Vocab` field's raw id bytes with its LABEL (ADR-0032),
/// reading the field's own data vocabulary through `store`. The pure decoder
/// cannot do this (no store), so the collection runs this right after decoding:
/// a `Value::Bytes` for a Vocab field means "id bytes", and an id the vocabulary
/// does not know STAYS bytes — the reader never invents a label.
pub fn resolve_vocab_labels<S: okm_core::VirtualStorage + ?Sized>(
    schema: &CollectionSchema,
    store: &S,
    header: &[u8],
    values: &mut ValueMap,
) {
    for f in &schema.hot_fields {
        let FieldType::Vocab(id_slot) = f.ty else {
            continue;
        };
        let Some(Value::Bytes(raw)) = values.get(&f.name) else {
            continue;
        };
        let faces = okm_core::obj_vocab::VocabFaces::from_id_slot(id_slot);
        if let Some(label) = okm_core::obj_vocab::vocab_label(store, header, faces, raw) {
            values.insert(f.name.clone(), Value::Str(label));
        }
    }
}

/// Decode the payload bytes (the two-segment region, no key bytes):
/// header version checked against the schema, hot fields sliced at
/// static offsets, cold TLV frames matched by tag. Unknown tags are
/// skipped — forward-compatible reading of fields the schema does not
/// know is the TLV contract (ADR-0004).
pub fn decode_payload(schema: &CollectionSchema, bytes: &[u8]) -> Result<ValueMap, CodecError> {
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
        match hot.get(f.offset..f.offset + f.width) {
            Some(raw) => {
                out.insert(f.name.clone(), decode_fixed(f.name.clone(), f.ty, f.width, raw)?);
            }
            // Truncated tail: field appended after this payload's hot
            // segment was written → version-default fill (below), not an
            // error. Mirrors the Rust decoder's truncation rule.
            None => continue,
        }
    }

    // Cold TLV frames: tag u8 + len varint + value, until the tail.
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
        let rest = bytes.get(pos..).ok_or_else(|| CodecError::Truncated {
            field: "<cold len>".into(),
            needed: 1,
            got: 0,
        })?;
        let Some((len, len_n)) = okm_core::take_len(rest) else {
            return Err(CodecError::Truncated {
                field: "<cold len>".into(),
                needed: 4,
                got: bytes.len() - pos,
            });
        };
        pos += len_n;
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
                    FieldType::Vector { .. } => {
                        // Frame = [count u32 BE][elements per T]. The
                        // element decoding is consumer-side (the value
                        // stays an opaque Bytes frame); the schema-side
                        // expect_len contract is enforced HERE.
                        if value.len() < 4 {
                            return Err(CodecError::Truncated {
                                field: f.name.clone(),
                                needed: 4,
                                got: value.len(),
                            });
                        }
                        let count = u32::from_be_bytes(value[0..4].try_into().unwrap());
                        if let Some(expect) = f.expect_len
                            && count as usize != expect {
                                return Err(CodecError::TypeMismatch {
                                    field: f.name.clone(),
                                    expected: "element count matching expect_len",
                                });
                            }
                        Value::Bytes(value.to_vec())
                    }
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
    // Version-default migration: a payload written by an older layout
    // lacks tail fields; the dynamic reader fills them from the schema's
    // exported defaults (mirrors the Rust decoder's #[ok_default]/Default
    // rule). Zero fallback when the export carries no literal.
    fill_schema_defaults(schema, &mut out);
    Ok(out)
}

/// Fill every declared payload field the map does not carry with the schema's
/// default: the exported literal when `CollectionSchema` has one, else the
/// kind's zero image (a `Vector` gets an empty frame, a `Vocab` a zero id).
///
/// ONE definition, two readers: [`decode_payload`]'s version migration and the
/// whole-document `replace` arm (`plan_replace`), which rebuilds a row from a
/// name-keyed map that may name only some declared fields. A map that skips a
/// field is not an error — the payload is positional and a half row cannot be
/// encoded — so the default is what "absent" means on both paths.
pub(crate) fn fill_schema_defaults(schema: &CollectionSchema, out: &mut ValueMap) {
    for f in schema.hot_fields.iter().chain(schema.cold_fields.iter()) {
        if out.contains_key(&f.name) {
            continue;
        }
        let v = match &f.default {
            Some(okm_core::schema::DefaultValue::U64(x)) => Value::U64(*x),
            Some(okm_core::schema::DefaultValue::I64(x)) => Value::I64(*x),
            Some(okm_core::schema::DefaultValue::F64(x)) => Value::F64(*x),
            Some(okm_core::schema::DefaultValue::Bool(x)) => Value::Bool(*x),
            Some(okm_core::schema::DefaultValue::Str(x)) => Value::Str(x.clone()),
            Some(okm_core::schema::DefaultValue::ZeroBytes(n)) => Value::Bytes(vec![0; *n]),
            None => match f.ty {
                FieldType::U8 => Value::U8(0),
                FieldType::U16 => Value::U16(0),
                FieldType::U32 => Value::U32(0),
                FieldType::U64 | FieldType::VarInt | FieldType::Quant(_) | FieldType::Offset(_) => {
                    Value::U64(0)
                }
                FieldType::Str => Value::Str(String::new()),
                FieldType::Bytes => Value::Bytes(vec![0; f.width]),
                FieldType::FixedBytes => Value::Bytes(vec![0; f.width]),
                // ADR-0032: absent Vocab field → a zero id image of the declared
                // width; `resolve_vocab_labels` turns it into a label when the
                // vocabulary has id 0.
                FieldType::Vocab(_) => Value::Bytes(vec![0; f.width]),
                // Vector default = empty frame (count 0) — the natural
                // default for a variable-length list.
                FieldType::Vector { .. } => Value::Bytes(vec![0, 0, 0, 0]),
            },
        };
        out.insert(f.name.clone(), v);
    }
}

pub(crate) fn decode_fixed(
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
        // ADR-0032: the id in the field's data vocabulary, handed out as raw
        // bytes — `resolve_vocab_labels` (store-aware, run by the collection)
        // replaces it with the label. This decoder has no store, so it never
        // invents one.
        FieldType::Vocab(_) => Value::Bytes(raw.to_vec()),
        // Str/Bytes are cold-only; hot segment never carries them (width 0).
        // Vector is hot-capable but decoded via its own arm below — the
        // generic fixed walk treats it as raw bytes.
        FieldType::Vector { .. } => Value::Bytes(raw.to_vec()), // frame payload as-is
        FieldType::Str | FieldType::Bytes | FieldType::VarInt | FieldType::Quant(_)
        | FieldType::Offset(_) => {
            return Err(CodecError::TypeMismatch {
                field: name,
                expected: "hot-capable fixed-width kind",
            })
        }
    })
}
