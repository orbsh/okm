//! The plan surface for the dynamic segment (ADR-0037 4.16a): the
//! fields ops as pure functions over caller-held state — vocabulary
//! mirror in, ops out, no engine access. The embedded paths
//! (`DynamicCollection::put_fields` / `get_fields`) load a mirror from
//! their store and call the same functions, so a remote plan and the
//! embedded replay land byte-identical by construction (the documented
//! plan+replay discipline of `plan.rs`, extended from the typed segment
//! to the dynamic one).
//!
//! The binding shape this enables: a host-language binding holds NO
//! engine and NO bytes-level layout knowledge — it calls the plan
//! surface and ships the ops over its injection channel (one submit
//! callback + one read callback). The vocabulary, the nTLV frames, the
//! slot constants — every byte rule stays here, one source.

use okm_core::model::obj_dynamic::{decode_named, put_frame_named};
use okm_core::model::index::{DYNAMIC_SLOT, FIELD_VOCAB_ID_SLOT, FIELD_VOCAB_NAME_SLOT};
use okm_core::storage::{scan_suffix_kv, VirtualStorage};

use crate::{Value, ValueMap};

/// The field-name vocabulary as a pure mirror (no engine): the plan
/// surface's vocabulary carrier. `next_id` is the allocation watermark;
/// `written` collects what the last plan allocated (the caller adopts
/// it via `adopt` so the next plan sees the names without a reload).
#[derive(Debug, Clone, Default)]
pub struct VocabMirror {
    by_name: std::collections::BTreeMap<String, u16>,
    by_id: std::collections::BTreeMap<u16, String>,
    next_id: u16,
    written: Vec<(String, u16)>,
}

impl VocabMirror {
    /// Both directions of one table's vocabulary from the engine, plus the
    /// allocation cursor when the store has one (ADR-0032). Without a cursor —
    /// a vocabulary written before it existed — the watermark falls back to
    /// max + 1, which is what the scans below compute anyway.
    pub fn load<S: VirtualStorage>(store: &S, ns: &[u8]) -> Self {
        let mut d = Self::default();
        let mut p2 = ns.to_vec();
        okm_core::write_slot(&mut p2, FIELD_VOCAB_ID_SLOT);
        for (suffix, name) in scan_suffix_kv(store, &p2) {
            if suffix.len() != 2 {
                continue;
            }
            let id = u16::from_be_bytes([suffix[0], suffix[1]]);
            if let Ok(n) = String::from_utf8(name) {
                d.by_id.insert(id, n);
            }
        }
        let mut p3 = ns.to_vec();
        okm_core::write_slot(&mut p3, FIELD_VOCAB_NAME_SLOT);
        for (suffix, idv) in scan_suffix_kv(store, &p3) {
            if let Ok(n) = String::from_utf8(suffix)
                && idv.len() == 2 {
                    let id = u16::from_be_bytes([idv[0], idv[1]]);
                    d.by_name.insert(n, id);
                    if id >= d.next_id {
                        d.next_id = id + 1;
                    }
                }
        }
        // The cursor is the id face's bare-prefix singleton: `[ns][0x02]` with
        // no id bytes (ADR-0032) — unrepresentable as an id entry, so the two
        // cannot collide.
        let mut cursor_key = ns.to_vec();
        okm_core::write_slot(&mut cursor_key, FIELD_VOCAB_ID_SLOT);
        if let Some(raw) = store.get(&cursor_key)
            && raw.len() == 2 {
            let cursor = u16::from_be_bytes([raw[0], raw[1]]);
            d.next_id = d.next_id.max(cursor);
        }
        d
    }

    /// Build from caller-held pairs (the remote binding's receipt
    /// adoption: cache + plan receipt, no engine).
    pub fn from_pairs<'a, I: IntoIterator<Item = (&'a str, u16)>>(entries: I) -> Self {
        let mut d = Self::default();
        d.adopt_entries(entries.into_iter().map(|(n, i)| (n.to_string(), i)));
        d
    }

    /// Adopt one plan's `new_vocab_entries` receipt into the live cache
    /// (the remote caller folds it back before the next plan; the
    /// embedded replay's mirror already holds it — the plan allocated
    /// through the same map).
    pub fn adopt(&mut self, entries: &[(String, u16)]) {
        self.adopt_entries(entries.iter().cloned());
    }

    fn adopt_entries<I: IntoIterator<Item = (String, u16)>>(&mut self, entries: I) {
        for (name, id) in entries {
            self.by_name.insert(name.clone(), id);
            self.by_id.insert(id, name);
            if id >= self.next_id {
                self.next_id = id + 1;
            }
        }
    }

    /// Non-allocating probe (the read surface resolves names, never
    /// allocates ids).
    pub fn name_for(&self, id: u16) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }
}

/// One planned dynamic-segment write: the field entry plus the
/// vocabulary growth that first-seen names require. The ops are
/// MemBatch-shaped (`None` = delete): replay or ship as one wire frame
/// (ADR-0010's single `commit_batch`) — both land byte-identical.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFields {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// Names this plan allocated (id assignment order): the caller
    /// adopts them into its live mirror (`VocabMirror::adopt`) so the
    /// next plan sees them without a reload.
    pub new_vocab_entries: Vec<(String, u16)>,
}

/// The dynamic-segment entry key: `[ns][DYNAMIC_SLOT][pkey]` — one
/// layout, shared by the plan surface and the collection's embedded
/// path (the typed model's fields_key is byte-identical).
pub fn fields_key(ns: &[u8], pkey: &[u8]) -> Vec<u8> {
    let mut buf = ns.to_vec();
    okm_core::write_slot(&mut buf, DYNAMIC_SLOT);
    buf.extend_from_slice(pkey);
    buf
}

/// Plan one dynamic-field replacement WITHOUT touching an engine.
/// `vocab` is the caller's vocabulary mirror; first-seen names allocate
/// through it and grow the plan's vocabulary ops (both directions, the
/// same batch-consistency shape `VocabCache::id_for` writes). An empty
/// map deletes the entry (the whole-entry-replace rule of `put_fields`,
/// mirrored here). Nested `Obj` values share the vocabulary, recursed
/// by `put_frame_named`.
pub fn plan_put_fields(
    ns: &[u8],
    pkey: &[u8],
    fields: &ValueMap,
    vocab: &mut VocabMirror,
) -> Result<PlannedFields, String> {
    // The vocabulary mutation and the frame body interleave (a nested
    // Obj allocates mid-encode), so the resolver borrows the mirror
    // mutably for the whole encode; the ops it collects ride out with
    // the plan.
    let mut vocab_ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut body = Vec::new();
    {
        let by_name = &mut vocab.by_name;
        let by_id = &mut vocab.by_id;
        let next_id = &mut vocab.next_id;
        let written = &mut vocab.written;
        let mut resolver = |name: &str| -> u16 {
            if let Some(id) = by_name.get(name) {
                return *id;
            }
            let id = *next_id;
            *next_id += 1;
            by_name.insert(name.to_string(), id);
            by_id.insert(id, name.to_string());
            written.push((name.to_string(), id));
            let mut id_key = ns.to_vec();
            okm_core::write_slot(&mut id_key, FIELD_VOCAB_ID_SLOT);
            id_key.extend_from_slice(&id.to_be_bytes());
            vocab_ops.push((id_key, Some(name.as_bytes().to_vec())));
            let mut name_key = ns.to_vec();
            okm_core::write_slot(&mut name_key, FIELD_VOCAB_NAME_SLOT);
            name_key.extend_from_slice(name.as_bytes());
            vocab_ops.push((name_key, Some(id.to_be_bytes().to_vec())));
            id
        };
        for (name, value) in fields {
            let dv = value_to_dyn_value(value)?;
            put_frame_named(&mut body, name, &dv, &mut resolver);
        }
    }
    // The allocation cursor, exactly as `VocabCache::id_for` writes it: without
    // this op the plan's vocabulary and the typed path's vocabulary diverge, and
    // the next allocation (on either side) would hand out a duplicate id.
    if !vocab.written.is_empty() {
        let mut cursor_key = ns.to_vec();
        okm_core::write_slot(&mut cursor_key, FIELD_VOCAB_ID_SLOT);
        vocab_ops.push((cursor_key, Some(vocab.next_id.to_be_bytes().to_vec())));
    }
    let k = fields_key(ns, pkey);
    if body.is_empty() {
        vocab_ops.push((k, None));
    } else {
        vocab_ops.push((k, Some(body)));
    }
    Ok(PlannedFields {
        ops: vocab_ops,
        new_vocab_entries: std::mem::take(&mut vocab.written),
    })
}

/// Plan the dynamic-segment removal (declared fields untouched). The
/// vocabulary stays (names are append-only — a dangling vocabulary
/// entry is harmless, the documented rule).
pub fn plan_delete_fields(ns: &[u8], pkey: &[u8]) -> PlannedFields {
    PlannedFields {
        ops: vec![(fields_key(ns, pkey), None)],
        new_vocab_entries: Vec::new(),
    }
}

/// Resolve stored dynamic frames to a name-keyed map with a caller-held
/// mirror (the remote read surface; the embedded `get_fields` loads a
/// mirror from its engine and calls this). Unknown ids drop the field —
/// the vocabulary-absent rule `get_fields` records (single-writer
/// cannot hit it; the embedded path reloads the mirror per call, so
/// even shared-engine catch-up lands in the fresh load).
pub fn fields_from_frames(raw: &[u8], vocab: &VocabMirror) -> Result<ValueMap, String> {
    let frames = decode_named(raw, &mut |id| vocab.name_for(id).map(str::to_string));
    let mut out = ValueMap::new();
    for f in frames {
        let Some(name) = vocab.name_for(f.id) else {
            continue; // vocabulary entry absent: drop
        };
        let v = dyn_value_to_value(&f.value)?;
        out.insert(name.to_string(), v);
    }
    Ok(out)
}

/// okm-dynamic `Value` -> okm-core `DynamicValue` (the dynamic
/// segment's currency). Composites map natively (the dynamic segment is
/// the schema-free zone).
pub(crate) fn value_to_dyn_value(v: &Value) -> Result<okm_core::model::obj_dynamic::DynamicValue, String> {
    use okm_core::model::obj_dynamic::DynamicValue;
    Ok(match v {
        Value::U8(x) => DynamicValue::UInt(*x as u64),
        Value::U16(x) => DynamicValue::UInt(*x as u64),
        Value::U32(x) => DynamicValue::UInt(*x as u64),
        Value::U64(x) => DynamicValue::UInt(*x),
        Value::I64(x) => DynamicValue::Int(*x),
        Value::F64(x) => DynamicValue::F64(*x),
        Value::Bool(x) => DynamicValue::Bool(*x),
        Value::Str(s) => DynamicValue::Str(s.clone()),
        Value::Bytes(b) => DynamicValue::Bytes(b.clone()),
        Value::Null => DynamicValue::Null,
        Value::Obj(m) => {
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in m {
                out.insert(k.clone(), value_to_dyn_value(v)?);
            }
            DynamicValue::Obj(out)
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                out.push(value_to_dyn_value(v)?);
            }
            DynamicValue::Array(out)
        }
    })
}

/// okm-core `DynamicValue` -> okm-dynamic `Value`.
pub(crate) fn dyn_value_to_value(
    v: &okm_core::model::obj_dynamic::DynamicValue,
) -> Result<Value, String> {
    Ok(match v {
        okm_core::model::obj_dynamic::DynamicValue::UInt(x) => Value::U64(*x),
        okm_core::model::obj_dynamic::DynamicValue::Int(x) => Value::I64(*x),
        okm_core::model::obj_dynamic::DynamicValue::F64(x) => Value::F64(*x),
        okm_core::model::obj_dynamic::DynamicValue::Str(s) => Value::Str(s.clone()),
        okm_core::model::obj_dynamic::DynamicValue::Bytes(b) => Value::Bytes(b.clone()),
        okm_core::model::obj_dynamic::DynamicValue::Bool(x) => Value::Bool(*x),
        okm_core::model::obj_dynamic::DynamicValue::Null => Value::Null,
        okm_core::model::obj_dynamic::DynamicValue::Obj(m) => {
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in m {
                out.insert(k.clone(), dyn_value_to_value(v)?);
            }
            Value::Obj(out)
        }
        okm_core::model::obj_dynamic::DynamicValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                out.push(dyn_value_to_value(v)?);
            }
            Value::Array(out)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> Vec<u8> {
        vec![0u8, 7]
    }

    fn two_fields() -> ValueMap {
        let mut f = ValueMap::new();
        f.insert("note".into(), Value::Str("hello".into()));
        f.insert("weight".into(), Value::U64(9));
        f
    }

    #[test]
    fn plan_allocates_both_directions_then_the_entry() {
        let mut vocab = VocabMirror::default();
        let plan = plan_put_fields(&ns(), b"k", &two_fields(), &mut vocab).unwrap();
        // ValueMap order: note < weight → ids 0 and 1.
        assert_eq!(
            plan.new_vocab_entries,
            vec![("note".to_string(), 0u16), ("weight".to_string(), 1u16)]
        );
        // 2 names x 2 directions + the cursor + the entry = 6 ops, entry LAST.
        assert_eq!(plan.ops.len(), 6);
        assert_eq!(plan.ops[5].0, fields_key(&ns(), b"k"));
        assert!(plan.ops[5].1.is_some());
    }

    #[test]
    fn known_names_write_no_vocab_ops_and_watermark_continues() {
        let mut vocab = VocabMirror::from_pairs([("note", 0u16), ("weight", 1u16)]);
        let plan = plan_put_fields(&ns(), b"k", &two_fields(), &mut vocab).unwrap();
        assert!(plan.new_vocab_entries.is_empty());
        assert_eq!(plan.ops.len(), 1);
        // After adoption, a fresh name continues from the watermark.
        vocab.adopt(&[("extra".to_string(), 5u16)]);
        let mut f = ValueMap::new();
        f.insert("brand_new".into(), Value::Bool(true));
        let p2 = plan_put_fields(&ns(), b"k", &f, &mut vocab).unwrap();
        assert_eq!(p2.new_vocab_entries, vec![("brand_new".to_string(), 6u16)]);
    }

    #[test]
    fn empty_map_plans_the_delete() {
        let mut vocab = VocabMirror::default();
        let plan = plan_put_fields(&ns(), b"k", &ValueMap::new(), &mut vocab).unwrap();
        assert_eq!(plan.ops.len(), 1);
        assert!(plan.ops[0].1.is_none());
    }

    #[test]
    fn roundtrip_frames_to_map() {
        let mut vocab = VocabMirror::default();
        let plan = plan_put_fields(&ns(), b"k", &two_fields(), &mut vocab).unwrap();
        let body = plan.ops.last().unwrap().1.clone().unwrap();
        let back = fields_from_frames(&body, &vocab).unwrap();
        assert_eq!(back, two_fields());
    }
}
