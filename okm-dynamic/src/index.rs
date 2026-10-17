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

use okm_core::schema::CollectionSchema;
use okm_core::storage::VirtualStorage;
use crate::path::Path;
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
    /// Path index (ADR-0029): the indexed segment is resolved by walking
    /// `path` over a document, one entry per resolved value (fan-out over
    /// arrays), encoded per the declared `value_ty`.
    ///
    /// Maintained by the DYNAMIC-SEGMENT write path
    /// ([`path_index_entries`]) — the primary document holds
    /// schema-declared fields only, where a path of depth 1 is exactly
    /// what `Plain` already indexes. `fields` stays empty (the path is
    /// the source); the entry key does NOT carry the path — the slot is
    /// the path's identity.
    Path { path: Path, value_ty: okm_core::field::FieldType },
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
            AccessMethodKind::Plain => "plain".to_string(),
            AccessMethodKind::Partial(_) => "partial".to_string(),
            AccessMethodKind::Func(_) => "func".to_string(),
            AccessMethodKind::Path { path, value_ty } => {
                format!("path({:?} -> {value_ty:?})", path.components())
            }
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

    /// A path index over the schema-free zone (ADR-0029): `path` resolves
    /// in the dynamic segment, one entry per resolved value.
    ///
    /// The value type is DECLARED, never inferred: the entry segment is
    /// byte-ordered, so the encoding needs a type at declaration time
    /// (the primary document gets it from `FieldSchema`; a path has no
    /// schema field to consult). Only types the fixed-width/variable-
    /// width encoders cover are accepted — a wrapper-like type
    /// (`VarInt`/`Quant`/`Offset`/`Enum`/`Vector`) is rejected here
    /// rather than failing at every write.
    pub fn path(
        slot: u16,
        path: Path,
        value_ty: okm_core::field::FieldType,
    ) -> Result<Self, String> {
        use okm_core::field::FieldType as FT;
        if path.is_empty() {
            return Err("path index requires a non-empty path".to_string());
        }
        match value_ty {
            FT::U8 | FT::U16 | FT::U32 | FT::U64 | FT::FixedBytes | FT::Str => {}
            other => {
                return Err(format!(
                    "path index value type {other:?} has no declared encoding; \
                     declare the storage-form type (U8/U16/U32/U64/FixedBytes/Str)"
                ));
            }
        }
        Ok(Self {
            slot,
            fields: Vec::new(),
            includes: Vec::new(),
            kind: AccessMethodKind::Path { path, value_ty },
        })
    }

    /// Width of the indexed-field segment's FIXED-WIDTH head (schema-
    /// derived). A variable-width field is allowed as the LAST field
    /// (the okm-derive rule, mirrored here): the primary key is cut from
    /// the entry's END (`key_len` fixed bytes), so a trailing variable-
    /// width segment still locates — raw UTF-8, no length prefix
    /// (text-first regime, `IndexFuncResult for String`). A variable-
    /// width field anywhere but last cannot be located (no static width),
    /// and a second one has nothing to cut against — both are declaration
    /// errors, same as the derive.
    pub fn fields_width(&self, schema: &CollectionSchema) -> Result<usize, String> {
        Self::check_field_order(schema, &self.fields)?;
        let mut w = 0;
        for name in &self.fields {
            let f = find_field(schema, name)
                .ok_or_else(|| format!("access method field `{name}` not in schema"))?;
            if f.width == 0 {
                break; // the trailing variable-width tail: raw bytes after this
            }
            w += f.width;
        }
        Ok(w)
    }

    /// The derive's index-segment order rule, shared by fields AND
    /// includes (the value segment cuts the same way): variable-width
    /// fields at most one, last position only.
    fn check_field_order(schema: &CollectionSchema, names: &[String]) -> Result<(), String> {
        let mut variable_positions = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let f = find_field(schema, name)
                .ok_or_else(|| format!("access method field `{name}` not in schema"))?;
            if f.width == 0 {
                variable_positions.push(i);
            }
        }
        if let Some(pos) = variable_positions.first() {
            if *pos != names.len() - 1 {
                return Err(format!(
                    "access method field `{}` is variable-width; it must be the LAST field — fields after it cannot be located (no static width)",
                    names[*pos]
                ));
            }
            if variable_positions.len() > 1 {
                return Err(format!(
                    "at most one variable-width field allowed in an access method (found {})",
                    variable_positions.len()
                ));
            }
        }
        Ok(())
    }

    /// The indexed-field segment bytes, in declaration (sort) order.
    /// Payload-only (hot fields): the key IS the lookup target — indexing
    /// a key field would mean "use part of the pkey to find the pkey",
    /// which is what plain key-prefix scanning is for. Key/payload name
    /// collisions never silently resolve to the key side. A trailing
    /// variable-width field encodes as raw bytes (no frame — the pkey
    /// tail cuts it off, text-first).
    fn fields_bytes(
        &self,
        schema: &CollectionSchema,
        document: &ValueMap,
        pkey: &[u8],
    ) -> Result<Vec<u8>, String> {
        let _ = pkey; // payload-only; the pkey tail is appended by the caller
        Self::check_field_order(schema, &self.fields)?;
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
            if f.width == 0 {
                // Trailing variable-width tail: raw encoding, no frame
                // (the pkey tail cuts it — the derive's String regime).
                encode_variable(f.ty, v, &mut buf)?;
            } else {
                encode_fixed(f.ty, v, &mut buf)?;
            }
        }
        Ok(buf)
    }
}

/// Locate a field (key, hot, or cold) by name. Cold fields (variable-
/// width: Str et al.) are indexable as the TRAILING segment field — the
/// value lives in the document map, the segment carries raw bytes.
fn find_field<'s>(
    schema: &'s CollectionSchema,
    name: &str,
) -> Option<&'s okm_core::schema::FieldSchema> {
    schema
        .key_fields
        .iter()
        .chain(schema.hot_fields.iter())
        .chain(schema.cold_fields.iter())
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

/// Encoding for a variable-width field in a trailing index-segment tail
/// (the derive's String regime): raw UTF-8, NO length prefix — sort order
/// is byte-wise lexicographic and the pkey tail cuts the segment. Only
/// the trailing position ever calls this (`check_field_order` enforces).
fn encode_variable(
    ty: okm_core::field::FieldType,
    v: &Value,
    buf: &mut Vec<u8>,
) -> Result<(), String> {
    use okm_core::field::FieldType as FT;
    let unexpected = |want: &str| format!("field type {want:?} does not take {v:?}");
    match (ty, v) {
        (FT::Str, Value::Str(s)) => buf.extend_from_slice(s.as_bytes()),
        _ => return Err(unexpected("variable-width (Str)")),
    }
    Ok(())
}

/// All index entries one document produces, mirroring
/// `KvIndex::entry_pairs` for the dynamic declarations:
/// key = `[ns][slot][index fields][pkey]`, value = includes segment.
/// Partial indexes consult their predicate first (`false` → no entries);
/// function indexes fan out one entry per derived value (inverted-index
/// regime, the Rust-side `entry_pairs` multi-value shape).
/// Entry list shape: the `(key, value)` pairs one write set lands —
/// primary rows and index entries alike (ADR-0016 layouts).
pub type IndexEntries = Vec<(Vec<u8>, Vec<u8>)>;

pub fn index_entries(
    schema: &CollectionSchema,
    ns: &[u8],
    indexes: &[AccessMethod],
    pkey: &[u8],
    document: &ValueMap,
) -> Result<IndexEntries, String> {
    let mut out = Vec::new();
    for idx in indexes {
        // Partial gate (purity contract documented on `Admits`): consulted
        // before any entry is built, the single entry-production site, so
        // the filter is complete by construction.
        if let AccessMethodKind::Partial(admits) = &idx.kind
            && !admits(document)? {
            continue;
        }
        // Path indexes are maintained by the DYNAMIC-SEGMENT write path
        // (`path_index_entries`): the primary document carries
        // schema-declared fields only, where a depth-1 path is what
        // `Plain` already covers. Skipping here keeps exactly ONE
        // maintainer per entry set — generating them on both paths would
        // leave entries the dynamic sweep cannot see.
        if matches!(idx.kind, AccessMethodKind::Path { .. }) {
            continue;
        }
        // The indexed-field segment: declared fields for Plain/Partial;
        // the callable's encoded results for Func (fan-out).
        let segments: Vec<Vec<u8>> = match &idx.kind {
            AccessMethodKind::Func(derive) => derive(document)?,
            _ => vec![idx.fields_bytes(schema, document, pkey)?],
        };
        // Includes segment: raw encodings of the named payload fields,
        // in declaration order. Key fields rejected — same discipline as
        // the index segment: includes carry payload, not key bytes. The
        // order rule is shared with the index segment (at most one
        // variable-width field, last only).
        AccessMethod::check_field_order(schema, &idx.includes)?;
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
            if f.width == 0 {
                encode_variable(f.ty, v, &mut ev)?;
            } else {
                encode_fixed(f.ty, v, &mut ev)?;
            }
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

/// Path-index entries for one document (ADR-0029): the dynamic-segment
/// write path's entry production. Non-`Path` declarations are skipped,
/// so the caller passes its full declaration list unchanged.
///
/// Rules, every one inherited from a shipped shape:
///
/// - one entry per RESOLVED value — an array path fans out (the
///   multi-entry regime `Func` already runs);
/// - a resolved value whose variant does not match the DECLARED type
///   produces NO entry and no error (a heterogeneous array is normal
///   data; the "absent attribute values are empty results" rule);
/// - a missing path (absent name, out-of-range index, path ending at an
///   object) produces no entry;
/// - a `Str` value rides the trailing variable-width regime — raw
///   UTF-8, no length prefix, the pkey tail cutting the segment — the
///   same rule a `Plain` index's cold last field follows;
/// - the entry key does NOT carry the path: the slot is the path's
///   identity, so a declared path costs the same bytes as any index.
pub fn path_index_entries(
    _schema: &CollectionSchema,
    ns: &[u8],
    indexes: &[AccessMethod],
    pkey: &[u8],
    document: &ValueMap,
) -> Result<IndexEntries, String> {
    let mut out = Vec::new();
    for idx in indexes {
        let AccessMethodKind::Path { path, value_ty } = &idx.kind else {
            continue;
        };
        let mut segments: Vec<Vec<u8>> = Vec::new();
        for v in path.resolve(document) {
            let mut fb = Vec::new();
            if encode_index_value(*value_ty, v, &mut fb).is_ok() {
                segments.push(fb);
            }
        }
        for fb in segments {
            let mut ek = Vec::with_capacity(ns.len() + 2 + fb.len() + pkey.len());
            ek.extend_from_slice(ns);
            ek.extend_from_slice(&idx.slot.to_be_bytes());
            ek.extend_from_slice(&fb);
            ek.extend_from_slice(pkey);
            out.push((ek, Vec::new()));
        }
    }
    Ok(out)
}

/// Encode one resolved path value into the entry's indexed segment.
///
/// The unsigned kinds are coerced, not matched strictly: the dynamic READ
/// side collapses every unsigned kind to `U64` (`dyn_value_to_value`:
/// `DynamicValue::UInt` → `Value::U64`), so a value written as `U32(1)`
/// comes back as `U64(1)`. An entry's bytes must be a function of the
/// stored value alone — a strict variant match would make the written map
/// and the read-back map produce DIFFERENT entry keys, and the overwrite
/// sweep (which recomputes keys from the read-back map) would silently
/// leave the old entry behind. Out-of-range values produce no entry (the
/// same empty-result rule as a missing path).
fn encode_index_value(
    ty: okm_core::field::FieldType,
    v: &Value,
    buf: &mut Vec<u8>,
) -> Result<(), String> {
    use okm_core::field::FieldType as FT;
    match ty {
        FT::U8 | FT::U16 | FT::U32 | FT::U64 => {
            let n = match v {
                Value::U8(n) => *n as u64,
                Value::U16(n) => *n as u64,
                Value::U32(n) => *n as u64,
                Value::U64(n) => *n,
                other => return Err(format!("field type {ty:?} does not take {other:?}")),
            };
            let narrowed = match ty {
                FT::U8 => Value::U8(u8::try_from(n).map_err(|_| format!("{n} overflows U8"))?),
                FT::U16 => Value::U16(u16::try_from(n).map_err(|_| format!("{n} overflows U16"))?),
                FT::U32 => Value::U32(u32::try_from(n).map_err(|_| format!("{n} overflows U32"))?),
                _ => Value::U64(n),
            };
            encode_fixed(ty, &narrowed, buf)
        }
        FT::Str => encode_variable(ty, v, buf),
        FT::FixedBytes => encode_fixed(ty, v, buf),
        other => Err(format!("path index value type {other:?} has no encoding")),
    }
}

/// Leftmost-prefix scan over one access method: returns the primary keys
/// of matching documents (dynamic counterpart of `okm_core::scan_index`).
/// The pkey is the tail of the entry key (`schema.key_len` bytes — a
/// dynamic index always indexes the FULL key; prefix keys are a
/// Rust-side refinement).
pub fn scan_access_method<S: VirtualStorage>(
    store: &S,
    schema: &CollectionSchema,
    ns: &[u8],
    index: &AccessMethod,
    encoded_prefix: &[u8],
) -> Result<Vec<ValueMap>, String> {
    // `encoded_prefix` is the leftmost prefix of the indexed-field
    // segment, already encoded (empty slice = whole index). Func
    // indexes have no declared-field width — the segment is the
    // callable's result encoding, caller-owned; any prefix is legal.
    // With a trailing VARIABLE-WIDTH field the probe may be LONGER than
    // the fixed-width head (the equality probe carries the full string,
    // the entry tail is raw UTF-8 — prefix comparison still matches);
    // a probe that exceeds the head is legal exactly when the
    // declaration's last field IS variable-width. A probe beyond the
    // head on an all-fixed-width declaration is a caller bug.
    if !matches!(
        index.kind,
        AccessMethodKind::Func(_) | AccessMethodKind::Path { .. }
    ) {
        let head = index.fields_width(schema)?;
        let trailing_variable = index
            .fields
            .last()
            .and_then(|n| find_field(schema, n))
            .map(|f| f.width == 0)
            .unwrap_or(false);
        if encoded_prefix.len() > head && !(trailing_variable && index.fields.len() == 1) {
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
        // A probe whose bytes ran past the entry's field segment into the
        // primary-key tail is not a match: the stored value is SHORTER than
        // the probe and cannot start with it. Such data is legal (values mix
        // letters and digits — stored `"ad"` + key `0x31…` vs the probe
        // `"ad1"`), so drop the entry instead of asserting.
        if suffix.len() < kl {
            continue;
        }
        let start = suffix.len() - kl;
        out.push(
            crate::decode_key(schema, &suffix[start..]).map_err(|e: crate::CodecError| e.to_string())?,
        );
    }
    Ok(out)
}

