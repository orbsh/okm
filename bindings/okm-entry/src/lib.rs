//! The shared injection machinery for the host bindings (ADR-0037
//! 4.16b) — the half of `okm-python`'s host-injected Collection face
//! that is pure Rust and belongs in ONE source: the byte-level engine
//! face (`Engine`/`EngineBox`) and the raw-entry builders
//! (`schema_of_entry`, `plain_access_method`, `preset_reduce`).
//!
//! Layout single-source discipline: bindings must not re-implement the
//! entry semantics any more than the byte layout — the python and steel
//! bindings build the same `Collection` face over the same plan
//! executor, from the same parsing of the interface_schema storage
//! entry. The error TYPE stays per binding (PyErr vs steel `String`);
//! this crate's errors are plain strings the shells wrap.

use okm_core::schema::CollectionSchema;
use okm_dynamic::{AccessMethod, AccessMethodKind};
use std::sync::Arc;

/// The injected engine face (ADR-0037 4.16a): a host (probe's carrier,
/// an embedded test harness) hands the collection a live engine behind
/// FOUR byte-level methods — the exact surface the plan paths consume
/// (get / scan / put / del). Bytes in, bytes out: the contract is
/// rev-independent (no okm types cross it), which is what lets the host
/// hold a different okm build than the binding. Semantics mirror
/// `VirtualStorage` (ordered keys, `None` end = unbounded, scan returns
/// FULL keys).
pub trait Engine: Send + Sync {
    fn put(&self, key: Vec<u8>, value: Vec<u8>);
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn del(&self, key: &[u8]);
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>>;
}

/// The collection's engine: the embedded mode's in-process store or an
/// injected host engine. One executor behind both faces (the 0037 §1
/// rule): same `DynamicCollection` code path, the enum only dispatches
/// bytes.
pub enum EngineBox {
    Test(okm_core::TestStore),
    Injected(Arc<dyn Engine>),
}

impl okm_core::storage::VirtualStorage for EngineBox {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        match self {
            Self::Test(s) => s.put(key, value),
            Self::Injected(e) => e.put(key, value),
        }
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::Test(s) => s.get(key),
            Self::Injected(e) => e.get(key),
        }
    }
    fn del(&self, key: &[u8]) {
        match self {
            Self::Test(s) => s.del(key),
            Self::Injected(e) => e.del(key),
        }
    }
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        match self {
            Self::Test(s) => s.scan_range(begin, end),
            Self::Injected(e) => e.scan_range(begin, end),
        }
    }
}

/// The RAW interface_schema storage entry → the collection's parts:
/// `{ "schema": <CollectionSchema>, "indexes": [{name, slot, fields,
/// includes, kind}], "reduces": [{name, slot, group, kind}] }` (the
/// bare schema object is also accepted). Declared indexes must be plain
/// (host callables — func/partial — register through the embedded
/// add_* surface instead); reduce kinds take the okm-dynamic preset
/// spellings ("count", {"high_water": f}, {"low_water": f}). ns is NOT
/// in the entry — the DSL's rule: it binds at construction, per
/// binding.
pub fn collection_from_entry(
    engine: EngineBox,
    entry: &serde_json::Value,
    ns: u16,
) -> Result<
    (
        CollectionSchema,
        okm_dynamic::DynamicCollection<EngineBox>,
    ),
    String,
> {
    let raw = entry.get("schema").cloned().unwrap_or_else(|| entry.clone());
    let schema: CollectionSchema =
        serde_json::from_value(raw).map_err(|e| format!("bad collection schema: {e}"))?;
    let mut indexes: Vec<AccessMethod> = Vec::new();
    if let Some(list) = entry.get("indexes").and_then(|x| x.as_array()) {
        for e in list {
            indexes.push(plain_access_method(e)?);
        }
    }
    let mut reduces: Vec<okm_dynamic::ReduceSpec> = Vec::new();
    if let Some(list) = entry.get("reduces").and_then(|x| x.as_array()) {
        for e in list {
            reduces.push(preset_reduce(e)?);
        }
    }
    let table = okm_dynamic::DynamicCollection::with_reduces(
        engine,
        ns,
        schema.clone(),
        indexes,
        reduces,
    );
    Ok((schema, table))
}

/// One declared plain index from the schema-data spelling (the entry
/// shape okm_schema.py assembles): routing is by slot; kind must be
/// plain — a host-callable index cannot be built from data (the
/// embedded add_* surface owns callables).
pub fn plain_access_method(v: &serde_json::Value) -> Result<AccessMethod, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "index entry must be an object".to_string())?;
    let slot = obj
        .get("slot")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| "index missing slot".to_string())?
        as u16;
    let strings = |arr: &serde_json::Value| -> Vec<String> {
        arr.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let fields = obj.get("fields").map(strings).ok_or_else(|| "index missing fields".to_string())?;
    let includes = obj.get("includes").map(strings).unwrap_or_default();
    let kind = obj
        .get("kind")
        .and_then(|x| x.as_str())
        .unwrap_or("plain");
    if kind != "plain" {
        return Err(format!(
            "index slot {slot}: kind `{kind}` needs a host callable — use the embedded add_* surface"
        ));
    }
    Ok(AccessMethod {
        slot,
        fields,
        includes,
        kind: AccessMethodKind::Plain,
    })
}

/// One declared preset reduce from the schema-data spelling:
/// `{ "slot": N, "group": [fields], "kind": "count" |
/// {"high_water": f} | {"low_water": f} }` — the okm-dynamic presets
/// (the u64 BE accumulator shared with the derive and the aura
/// executor: one implementation, every host).
pub fn preset_reduce(v: &serde_json::Value) -> Result<okm_dynamic::ReduceSpec, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "reduce entry must be an object".to_string())?;
    let slot = obj
        .get("slot")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| "reduce missing slot".to_string())?
        as u16;
    let group_fields = obj
        .get("group")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let kind_v = obj
        .get("kind")
        .ok_or_else(|| "reduce missing kind".to_string())?;
    let kind = if let Some(s) = kind_v.as_str() {
        match s {
            "count" => okm_dynamic::PresetKind::Count,
            other => return Err(format!("unknown reduce kind `{other}`")),
        }
    } else if let Some(o) = kind_v.as_object().filter(|o| o.len() == 1) {
        let (k, f) = o.iter().next().unwrap();
        let field = f
            .as_str()
            .ok_or_else(|| "reduce kind field must be a string".to_string())?
            .to_string();
        match k.as_str() {
            "high_water" => okm_dynamic::PresetKind::HighWater(field),
            "low_water" => okm_dynamic::PresetKind::LowWater(field),
            other => return Err(format!("unknown reduce kind `{other}`")),
        }
    } else {
        return Err("reduce kind must be a string or {kind: field}".to_string());
    };
    Ok(okm_dynamic::ReduceSpec {
        slot,
        group_fields,
        logic: Box::new(okm_dynamic::PresetLogic { kind }),
    })
}
