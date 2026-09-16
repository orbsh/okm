//! Runtime-typed access methods for dynamic tables (schema-declared
//! secondary indexes, mirroring `#[ok_index]`).
//!
//! A declared access method has a slot (1-based, matching the static
//! allocation rule: declaration order), indexed payload/key fields, and an
//! optional includes list. Entries share the byte layout of the derive:
//! `[ns 2B][slot][index fields][key prefix]` → value = includes segment.
//!
//! Capability ceiling (permanent, per ADR-0008): no function indexes, no
//! multi-entry fanout, no reduce/subscribe — a dynamic rebuild of those
//! would break exactly-once. Plain field indexes only.

use okm_core::schema::TableSchema;
use okm_core::storage::VirtualStorage;
use crate::{Value, ValueMap};

/// One declared access method over a dynamic table.
#[derive(Clone, Debug, PartialEq)]
pub struct AccessMethod {
    /// Entry header slot byte (1, 2, …; 0 is reserved for the primary
    /// entry — the same allocation rule as the derive).
    pub slot: u8,
    /// Indexed fields by schema name, in sort order. Fields may live in
    /// the key segment or the hot payload segment (both are fixed-width,
    /// static-placement — everything an entry needs).
    pub fields: Vec<String>,
    /// Payload fields carried in the entry value (raw encodings,
    /// concatenated — the derive's `includes`).
    pub includes: Vec<String>,
}

impl AccessMethod {
    /// Width of the indexed-field segment (schema-derived; every field
    /// must be fixed-width — dynamic entries cannot frame variable
    /// lengths without breaking leftmost-prefix scans).
    pub fn fields_width(&self, schema: &TableSchema) -> Result<usize, String> {
        let mut w = 0;
        for name in &self.fields {
            let f = find_field(schema, name)
                .ok_or_else(|| format!("access method field `{name}` not in schema"))?;
            if f.width == 0 {
                return Err(format!(
                    "access method field `{name}` is variable-width; dynamic indexes take fixed-width fields only"
                ));
            }
            w += f.width;
        }
        Ok(w)
    }

    /// The indexed-field segment bytes, in declaration (sort) order.
    /// Payload-only (hot fields): the key IS the lookup target — indexing
    /// a key field would mean "use part of the pkey to find the pkey",
    /// which is what plain key-prefix scanning is for. Key/payload name
    /// collisions never silently resolve to the key side.
    fn fields_bytes(
        &self,
        schema: &TableSchema,
        row: &ValueMap,
        pkey: &[u8],
    ) -> Result<Vec<u8>, String> {
        let _ = pkey; // payload-only; the pkey tail is appended by the caller
        let mut buf = Vec::new();
        for name in &self.fields {
            if schema.key_fields.iter().any(|f| &f.name == name) {
                return Err(format!(
                    "access method field `{name}` is a key field; indexes take payload fields only"
                ));
            }
            let v = row.get(name)
                .ok_or_else(|| format!("index field `{name}` missing from row"))?;
            let f = find_field(schema, name)
                .ok_or_else(|| format!("access method field `{name}` not in schema"))?;
            encode_fixed(f.ty, v, &mut buf)?;
        }
        Ok(buf)
    }
}

/// Locate a fixed-width field (key or hot) by name.
fn find_field<'s>(
    schema: &'s TableSchema,
    name: &str,
) -> Option<&'s okm_core::schema::FieldSchema> {
    schema
        .key_fields
        .iter()
        .chain(schema.hot_fields.iter())
        .find(|f| f.name == name)
}

/// BE encoding for a fixed-width dynamic value (the derive's index
/// segment: plain big-endian bytes of the storage form).
fn encode_fixed(
    ty: okm_core::field::FieldType,
    v: &Value,
    buf: &mut Vec<u8>,
) -> Result<(), String> {
    use okm_core::field::FieldType as FT;
    let unexpected = |want: &str| format!("field type {want:?} does not take {v:?}");
    match (ty, v) {
        (FT::U8, Value::U8(n)) => buf.push(*n),
        (FT::U16, Value::U16(n)) => buf.extend_from_slice(&n.to_be_bytes()),
        (FT::U32, Value::U32(n)) => buf.extend_from_slice(&n.to_be_bytes()),
        (FT::U64, Value::U64(n)) => buf.extend_from_slice(&n.to_be_bytes()),
        (FT::FixedBytes, Value::Bytes(b)) => buf.extend_from_slice(b),
        _ => return Err(unexpected("fixed-width")),
    }
    Ok(())
}

/// All index entries one row produces, mirroring
/// `KvIndex::entry_pairs` for the dynamic declarations:
/// key = `[ns][slot][index fields][pkey]`, value = includes segment.
pub fn index_entries(
    schema: &TableSchema,
    ns: &[u8],
    indexes: &[AccessMethod],
    pkey: &[u8],
    row: &ValueMap,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
    let mut out = Vec::new();
    for idx in indexes {
        let fb = idx.fields_bytes(schema, row, pkey)?;
        let mut ek = Vec::with_capacity(ns.len() + 1 + fb.len() + pkey.len());
        ek.extend_from_slice(ns);
        ek.push(idx.slot);
        ek.extend_from_slice(&fb);
        ek.extend_from_slice(pkey);
        // Includes segment: raw encodings of the named payload fields,
        // in declaration order. Key fields rejected — same discipline as
        // the index segment: includes carry payload, not key bytes.
        let mut ev = Vec::new();
        for inc in &idx.includes {
            if schema.key_fields.iter().any(|f| &f.name == inc) {
                return Err(format!(
                    "includes field `{inc}` is a key field; includes take payload fields only"
                ));
            }
            let v = row.get(inc)
                .ok_or_else(|| format!("includes field `{inc}` missing from row"))?;
            let f = find_field(schema, inc)
                .ok_or_else(|| format!("includes field `{inc}` not in schema"))?;
            encode_fixed(f.ty, v, &mut ev)?;
        }
        out.push((ek, ev));
    }
    Ok(out)
}

/// Leftmost-prefix scan over one access method: returns the primary keys
/// of matching rows (dynamic counterpart of `okm_core::scan_index`).
/// The pkey is the tail of the entry key (`schema.key_len` bytes — a
/// dynamic index always indexes the FULL key; prefix keys are a
/// Rust-side refinement).
pub fn scan_access_method<S: VirtualStorage>(
    store: &S,
    schema: &TableSchema,
    ns: &[u8],
    index: &AccessMethod,
    encoded_prefix: &[u8],
) -> Result<Vec<ValueMap>, String> {
    // `encoded_prefix` is the leftmost prefix of the indexed-field
    // segment, already encoded (empty slice = whole index).
    if encoded_prefix.len() > index.fields_width(schema)? {
        return Err("scan prefix exceeds the index-field segment".into());
    }
    let mut p = Vec::with_capacity(ns.len() + 1 + encoded_prefix.len());
    p.extend_from_slice(ns);
    p.push(index.slot);
    p.extend_from_slice(encoded_prefix);
    let kl = schema.key_len;
    let mut out = Vec::new();
    for suffix in store.scan_suffix(&p) {
        assert!(suffix.len() >= kl, "index entry shorter than the primary key");
        let start = suffix.len() - kl;
        out.push(
            crate::decode_key(schema, &suffix[start..]).map_err(|e: crate::CodecError| e.to_string())?,
        );
    }
    Ok(out)
}

/// Stale-entry sweep: delete every entry under this access method whose
/// indexed values no longer match the current row (derive overwrite
/// lands stale entries at different keys; `delete` covers the rest).
/// Dynamic tables recompute entries per write, so the sweep runs on
/// every `DynamicTable::put` for the overwritten key's OLD entries.
pub fn delete_entries(
    store: &mut impl VirtualStorage,
    entries: &[(Vec<u8>, Vec<u8>)],
) {
    for (ek, _) in entries {
        store.del(ek);
    }
}
