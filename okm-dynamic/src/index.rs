//! Runtime-typed access methods for dynamic tables (schema-declared
//! secondary indexes, mirroring `#[ok_index]`).
//!
//! A declared access method has a slot (1-based, matching the static
//! allocation rule: declaration order), indexed payload/key fields, and an
//! optional includes list. Entries share the byte layout of the derive:
//! `[ns 2B][slot][index fields][key prefix]` → value = includes segment.
//!
//! Capability scope (ADR-0022): subscribe stays excluded; function/partial
//! indexes are binding-implementable — the callable (predicate or derive
//! function) runs in the host language at registration time and returns
//! ENCODED bytes; this module only manages the entry lifecycle.

use okm_core::schema::TableSchema;
use okm_core::storage::VirtualStorage;
use crate::{Value, ValueMap};

/// Host-language callables for the semantic access methods (ADR-0022).
/// Every callable receives the decoded document and returns encoded bytes
/// or an error — the dynamic layer never inspects semantics, it only
/// manages entries. Errors are ordinary input (dynamic-side discipline).
pub type CallableResult<T> = Result<T, String>;

/// Partial-index predicate: `false` = this document contributes NO
/// entries. Must be pure (same purity contract as the Rust-side
/// `admits`): an impure predicate makes delete compute a different
/// entry set than put and leaves dangling entries.
pub type Admits = Box<dyn Fn(&ValueMap) -> CallableResult<bool> + Send + Sync>;

/// Function-index derive: one encoded value per entry (plain func index),
/// or several (multi-entry fan-out, the inverted-index regime). Sort
/// order = the result encodings' order, same as the derive.
pub type FuncDerive = Box<dyn Fn(&ValueMap) -> CallableResult<Vec<Vec<u8>>> + Send + Sync>;

/// Which entries an access method produces.
pub enum AccessMethodKind {
    /// Plain field index (the shipped shape): `fields` + `includes`.
    Plain,
    /// Partial index: the plain layout, gated by a host predicate.
    Partial(Admits),
    /// Function index: the caller supplies the derive callable; `fields`
    /// is EMPTY (the derive result IS the indexed segment) and `includes`
    /// carries payload as usual.
    Func(FuncDerive),
}

/// One declared access method over a dynamic table.
pub struct AccessMethod {
    /// Entry header slot (index segment, u16: 0x1001, 0x1002, …; the
    /// primary is 0x0000 — ADR-0016).
    pub slot: u16,
    /// Indexed fields by schema name, in sort order. Fields may live in
    /// the key segment or the hot payload segment (both are fixed-width,
    /// static-placement — everything an entry needs). Empty for `Func`
    /// (the callable's encoded result IS the indexed segment).
    pub fields: Vec<String>,
    /// Payload fields carried in the entry value (raw encodings,
    /// concatenated — the derive's `includes`).
    pub includes: Vec<String>,
    /// Entry production rule (plain / partial / func, ADR-0022).
    pub kind: AccessMethodKind,
}

impl std::fmt::Debug for AccessMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.kind {
            AccessMethodKind::Plain => "plain",
            AccessMethodKind::Partial(_) => "partial",
            AccessMethodKind::Func(_) => "func",
        };
        f.debug_struct("AccessMethod")
            .field("slot", &self.slot)
            .field("fields", &self.fields)
            .field("includes", &self.includes)
            .field("kind", &kind)
            .finish()
    }
}

impl AccessMethod {
    /// A plain field index.
    pub fn plain(slot: u16, fields: Vec<String>, includes: Vec<String>) -> Self {
        Self { slot, fields, includes, kind: AccessMethodKind::Plain }
    }

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
        document: &ValueMap,
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
            let v = document.get(name)
                .ok_or_else(|| format!("index field `{name}` missing from document"))?;
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

/// All index entries one document produces, mirroring
/// `KvIndex::entry_pairs` for the dynamic declarations:
/// key = `[ns][slot][index fields][pkey]`, value = includes segment.
/// Partial indexes consult their predicate first (`false` → no entries);
/// function indexes fan out one entry per derived value (inverted-index
/// regime, the Rust-side `entry_pairs` multi-value shape).
pub fn index_entries(
    schema: &TableSchema,
    ns: &[u8],
    indexes: &[AccessMethod],
    pkey: &[u8],
    document: &ValueMap,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
    let mut out = Vec::new();
    for idx in indexes {
        // Partial gate (purity contract documented on `Admits`): consulted
        // before any entry is built, the single entry-production site, so
        // the filter is complete by construction.
        if let AccessMethodKind::Partial(admits) = &idx.kind {
            if !admits(document)? {
                continue;
            }
        }
        // The indexed-field segment: declared fields for Plain/Partial;
        // the callable's encoded results for Func (fan-out).
        let segments: Vec<Vec<u8>> = match &idx.kind {
            AccessMethodKind::Func(derive) => derive(document)?,
            _ => vec![idx.fields_bytes(schema, document, pkey)?],
        };
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
            let v = document.get(inc)
                .ok_or_else(|| format!("includes field `{inc}` missing from document"))?;
            let f = find_field(schema, inc)
                .ok_or_else(|| format!("includes field `{inc}` not in schema"))?;
            encode_fixed(f.ty, v, &mut ev)?;
        }
        for fb in &segments {
            let mut ek = Vec::with_capacity(ns.len() + 2 + fb.len() + pkey.len());
            ek.extend_from_slice(ns);
            ek.extend_from_slice(&idx.slot.to_be_bytes());
            ek.extend_from_slice(fb);
            ek.extend_from_slice(pkey);
            out.push((ek, ev.clone()));
        }
    }
    Ok(out)
}

/// Leftmost-prefix scan over one access method: returns the primary keys
/// of matching documents (dynamic counterpart of `okm_core::scan_index`).
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
    // segment, already encoded (empty slice = whole index). Func
    // indexes have no declared-field width — the segment is the
    // callable's result encoding, caller-owned; any prefix is legal.
    if !matches!(index.kind, AccessMethodKind::Func(_)) {
        if encoded_prefix.len() > index.fields_width(schema)? {
            return Err("scan prefix exceeds the index-field segment".into());
        }
    }
    let mut p = Vec::with_capacity(ns.len() + 2 + encoded_prefix.len());
    p.extend_from_slice(ns);
    p.extend_from_slice(&index.slot.to_be_bytes());
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
/// indexed values no longer match the current document (derive overwrite
/// lands stale entries at different keys; `delete` covers the rest).
/// Dynamic tables recompute entries per write, so the sweep runs on
/// every `DynamicCollection::put` for the overwritten key's OLD entries.
pub fn delete_entries(
    store: &mut impl VirtualStorage,
    entries: &[(Vec<u8>, Vec<u8>)],
) {
    for (ek, _) in entries {
        store.del(ek);
    }
}
