//! The plan surface for the dynamic segment (ADR-0037 4.16a): the
//! fields ops as pure functions over caller-held state — dictionary
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
//! callback + one read callback). The dictionary, the nTLV frames, the
//! slot constants — every byte rule stays here, one source.

use okm_core::model::obj_dynamic::{decode_named, put_frame_named};
use okm_core::model::index::{DICT_ID_SLOT, DICT_NAME_SLOT, DYNAMIC_SLOT};
use okm_core::storage::{scan_suffix_kv, VirtualStorage};

use crate::{Value, ValueMap};

/// The field-name dictionary as a pure mirror (no engine): the plan
/// surface's dictionary carrier. `next_id` is the allocation watermark;
/// `written` collects what the last plan allocated (the caller adopts
/// it via `adopt` so the next plan sees the names without a reload).
#[derive(Debug, Clone, Default)]
pub struct DictMirror {
    by_name: std::collections::BTreeMap<String, u16>,
    by_id: std::collections::BTreeMap<u16, String>,
    next_id: u16,
    written: Vec<(String, u16)>,
}

impl DictMirror {
    /// Both directions of one table's dictionary from the engine (one
    /// prefix scan each; the watermark derives from the mirror — max +
    /// 1, the load-once discipline `DictCache` records).
    pub fn load<S: VirtualStorage>(store: &S, ns: &[u8]) -> Self {
        let mut d = Self::default();
        let mut p2 = ns.to_vec();
        p2.extend_from_slice(&DICT_ID_SLOT.to_be_bytes());
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
        p3.extend_from_slice(&DICT_NAME_SLOT.to_be_bytes());
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
        d
    }

    /// Build from caller-held pairs (the remote binding's receipt
    /// adoption: cache + plan receipt, no engine).
    pub fn from_pairs<'a, I: IntoIterator<Item = (&'a str, u16)>>(entries: I) -> Self {
        let mut d = Self::default();
        d.adopt_entries(entries.into_iter().map(|(n, i)| (n.to_string(), i)));
        d
    }

    /// Adopt one plan's `new_dict_entries` receipt into the live cache
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
/// dictionary growth that first-seen names require. The ops are
/// MemBatch-shaped (`None` = delete): replay or ship as one wire frame
/// (ADR-0010's single `commit_batch`) — both land byte-identical.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFields {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// Names this plan allocated (id assignment order): the caller
    /// adopts them into its live mirror (`DictMirror::adopt`) so the
    /// next plan sees them without a reload.
    pub new_dict_entries: Vec<(String, u16)>,
}

/// The dynamic-segment entry key: `[ns][DYNAMIC_SLOT][pkey]` — one
/// layout, shared by the plan surface and the collection's embedded
/// path (the typed model's fields_key is byte-identical).
pub fn fields_key(ns: &[u8], pkey: &[u8]) -> Vec<u8> {
    let mut buf = ns.to_vec();
    buf.extend_from_slice(&DYNAMIC_SLOT.to_be_bytes());
    buf.extend_from_slice(pkey);
    buf
}

/// Plan one dynamic-field replacement WITHOUT touching an engine.
/// `dict` is the caller's dictionary mirror; first-seen names allocate
/// through it and grow the plan's dictionary ops (both directions, the
/// same batch-consistency shape `DictCache::id_for` writes). An empty
/// map deletes the entry (the whole-entry-replace rule of `put_fields`,
/// mirrored here). Nested `Obj` values share the dictionary, recursed
/// by `put_frame_named`.
pub fn plan_put_fields(
    ns: &[u8],
    pkey: &[u8],
    fields: &ValueMap,
    dict: &mut DictMirror,
) -> Result<PlannedFields, String> {
    // The dictionary mutation and the frame body interleave (a nested
    // Obj allocates mid-encode), so the resolver borrows the mirror
    // mutably for the whole encode; the ops it collects ride out with
    // the plan.
    let mut dict_ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut body = Vec::new();
    {
        let by_name = &mut dict.by_name;
        let by_id = &mut dict.by_id;
        let next_id = &mut dict.next_id;
        let written = &mut dict.written;
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
            id_key.extend_from_slice(&DICT_ID_SLOT.to_be_bytes());
            id_key.extend_from_slice(&id.to_be_bytes());
            dict_ops.push((id_key, Some(name.as_bytes().to_vec())));
            let mut name_key = ns.to_vec();
            name_key.extend_from_slice(&DICT_NAME_SLOT.to_be_bytes());
            name_key.extend_from_slice(name.as_bytes());
            dict_ops.push((name_key, Some(id.to_be_bytes().to_vec())));
            id
        };
        for (name, value) in fields {
            let dv = value_to_dyn_value(value)?;
            put_frame_named(&mut body, name, &dv, &mut resolver);
        }
    }
    let k = fields_key(ns, pkey);
    if body.is_empty() {
        dict_ops.push((k, None));
    } else {
        dict_ops.push((k, Some(body)));
    }
    Ok(PlannedFields {
        ops: dict_ops,
        new_dict_entries: std::mem::take(&mut dict.written),
    })
}

/// Plan the dynamic-segment removal (declared fields untouched). The
/// dictionary stays (names are append-only — a dangling dictionary
/// entry is harmless, the documented rule).
pub fn plan_delete_fields(ns: &[u8], pkey: &[u8]) -> PlannedFields {
    PlannedFields {
        ops: vec![(fields_key(ns, pkey), None)],
        new_dict_entries: Vec::new(),
    }
}

/// Resolve stored dynamic frames to a name-keyed map with a caller-held
/// mirror (the remote read surface; the embedded `get_fields` loads a
/// mirror from its engine and calls this). Unknown ids drop the field —
/// the dictionary-absent rule `get_fields` records (single-writer
/// cannot hit it; the embedded path reloads the mirror per call, so
/// even shared-engine catch-up lands in the fresh load).
pub fn fields_from_frames(raw: &[u8], dict: &DictMirror) -> Result<ValueMap, String> {
    let frames = decode_named(raw, &mut |id| dict.name_for(id).map(str::to_string));
    let mut out = ValueMap::new();
    for f in frames {
        let Some(name) = dict.name_for(f.id) else {
            continue; // dictionary entry absent: drop
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
        let mut dict = DictMirror::default();
        let plan = plan_put_fields(&ns(), b"k", &two_fields(), &mut dict).unwrap();
        // ValueMap order: note < weight → ids 0 and 1.
        assert_eq!(
            plan.new_dict_entries,
            vec![("note".to_string(), 0u16), ("weight".to_string(), 1u16)]
        );
        // 2 names x 2 directions + the entry = 5 ops, entry LAST.
        assert_eq!(plan.ops.len(), 5);
        assert_eq!(plan.ops[4].0, fields_key(&ns(), b"k"));
        assert!(plan.ops[4].1.is_some());
    }

    #[test]
    fn known_names_write_no_dict_ops_and_watermark_continues() {
        let mut dict = DictMirror::from_pairs([("note", 0u16), ("weight", 1u16)]);
        let plan = plan_put_fields(&ns(), b"k", &two_fields(), &mut dict).unwrap();
        assert!(plan.new_dict_entries.is_empty());
        assert_eq!(plan.ops.len(), 1);
        // After adoption, a fresh name continues from the watermark.
        dict.adopt(&[("extra".to_string(), 5u16)]);
        let mut f = ValueMap::new();
        f.insert("brand_new".into(), Value::Bool(true));
        let p2 = plan_put_fields(&ns(), b"k", &f, &mut dict).unwrap();
        assert_eq!(p2.new_dict_entries, vec![("brand_new".to_string(), 6u16)]);
    }

    #[test]
    fn empty_map_plans_the_delete() {
        let mut dict = DictMirror::default();
        let plan = plan_put_fields(&ns(), b"k", &ValueMap::new(), &mut dict).unwrap();
        assert_eq!(plan.ops.len(), 1);
        assert!(plan.ops[0].1.is_none());
    }

    #[test]
    fn roundtrip_frames_to_map() {
        let mut dict = DictMirror::default();
        let plan = plan_put_fields(&ns(), b"k", &two_fields(), &mut dict).unwrap();
        let body = plan.ops.last().unwrap().1.clone().unwrap();
        let back = fields_from_frames(&body, &dict).unwrap();
        assert_eq!(back, two_fields());
    }
}
