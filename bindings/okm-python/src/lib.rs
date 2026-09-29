//! PyO3 bridge for the ADR-0022 semantic surfaces: a Python-owned
//! DynamicCollection (embedded mode — Python is the only writer) with
//! binding-time registration of host callables.
//!
//! - `ReduceLogic` subclass → `ReduceSpec` (seed/fold/unfold mirror the
//!   Rust-side `ReduceLogic` + `ReduceCodec: Default` pair).
//! - `add_func_index(slot, func, includes=[])` / `add_partial_index(slot,
//!   fields, admits, includes=[])` — callables receive the decoded
//!   document dict and return encoded bytes / bool.
//!
//! Errors cross as Python ValueError (dynamic-side discipline: errors
//! are ordinary input).
pub mod pyo3_impl {
	use okm_core::storage::VirtualStorage;
	use okm_dynamic::{AccessMethod, AccessMethodKind, ReduceLogic, Value, ValueMap};
	use pyo3::exceptions::{PyTypeError, PyValueError};
	use pyo3::prelude::*;
	use std::sync::{Arc, Mutex};

	/// Convert a ValueMap (decoded document) into a Python dict.
	fn map_to_dict<'py>(py: Python<'py>, m: &ValueMap) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
	    use pyo3::conversion::IntoPyObject;
	    let d = pyo3::types::PyDict::new(py);
	    for (k, v) in m {
	        let value: pyo3::PyObject = match v {
	            Value::U8(x) => x.into_pyobject(py)?.unbind().into_any(),
	            Value::U16(x) => x.into_pyobject(py)?.unbind().into_any(),
	            Value::U32(x) => x.into_pyobject(py)?.unbind().into_any(),
	            Value::U64(x) => x.into_pyobject(py)?.unbind().into_any(),
	            Value::I64(x) => x.into_pyobject(py)?.unbind().into_any(),
	            Value::F64(x) => x.into_pyobject(py)?.unbind().into_any(),
	            Value::Bool(x) => (*x as u8).into_pyobject(py)?.unbind().into_any(),
	            Value::Null => py.None(),
	            Value::Str(s) => s.into_pyobject(py)?.unbind().into_any(),
	            Value::Bytes(b) => b.into_pyobject(py)?.unbind().into_any(),
	            // Nested composites (okm 0c2a354): Obj → dict, Array → list.
	            Value::Obj(fields) => value_to_py(py, &Value::Obj(fields.clone()))?,
	            Value::Array(items) => value_to_py(py, &Value::Array(items.clone()))?,
	        };
	        d.set_item(k, value)?;
	    }
	    Ok(d)
	}

	/// Value → python object (the composite arm map_to_dict needs: Obj
	/// recurses through a dict, Array maps element-wise).
	fn value_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<PyObject> {
	    Ok(match v {
	        Value::Obj(fields) => {
	            let inner: ValueMap = fields.clone();
	            map_to_dict(py, &inner)?.into_any().unbind()
	        }
	        Value::Array(items) => {
	            let list = pyo3::types::PyList::empty(py);
	            for item in items {
	                list.append(value_to_py(py, item)?)?;
	            }
	            list.into_any().unbind()
	        }
	        other => {
	            let one = ValueMap::from([("__v__".to_string(), other.clone())]);
	            let d = map_to_dict(py, &one)?;
	            d.get_item("__v__")?
	                .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("value roundtrip failed"))?
	                .unbind()
	        }
	    })
	}

	fn call_err(e: PyErr) -> String {
	    e.to_string()
	}

	fn py_to_string(py: Python<'_>, err: PyErr) -> String {
	    let v = err.value(py);
	    v.str().map(|s| s.to_string()).unwrap_or_default()
	}

	/// The Python `ReduceLogic` subclass held as a Rust-side trait object.
	struct PyReduce {
	    obj: Py<PyAny>,
	}

	impl ReduceLogic for PyReduce {
	    fn seed(&self) -> Vec<u8> {
	        Python::with_gil(|py| {
	            self.obj
	                .call_method(py, "seed", (), None)
	                .and_then(|b| b.extract::<Vec<u8>>(py))
	                .unwrap_or_default()
	        })
	    }

	    fn fold(&self, acc: &mut Vec<u8>, key: &ValueMap, document: &ValueMap) -> Result<(), String> {
	        Python::with_gil(|py| self.apply(py, "fold", acc, key, document))
	    }

	    fn unfold(&self, acc: &mut Vec<u8>, key: &ValueMap, document: &ValueMap) -> Result<(), String> {
	        Python::with_gil(|py| self.apply(py, "unfold", acc, key, document))
	    }
	}

	impl PyReduce {
	    fn apply(
	        &self,
	        py: Python<'_>,
	        method: &str,
	        acc: &mut Vec<u8>,
	        key: &ValueMap,
	        document: &ValueMap,
	    ) -> Result<(), String> {
	        let py_acc = pyo3::types::PyByteArray::new(py, acc);
	        // ADR-0024: the host callable sees the DECODED key dict — one
	        // object model, never bytes.
	        let py_key = map_to_dict(py, key).map_err(call_err)?;
	        let doc = map_to_dict(py, document).map_err(call_err)?;
	        self.obj
	            .call_method(py, method, (py_acc.clone(), py_key, doc), None)
	            .map_err(|e| format!("reduce {method}: {}", py_to_string(py, e)))?;
	        *acc = py_acc.to_vec();
	        Ok(())
	    }
	}

	/// The injected engine face (ADR-0037 4.16a): a host (probe's python
	/// carrier, an embedded test harness) hands the collection a live
	/// engine behind FOUR byte-level methods — the exact surface the
	/// plan paths consume (get / scan / put / del). Bytes in, bytes out:
	/// the contract is rev-independent (no okm types cross it), which is
	/// what lets the host hold a different okm build than the binding.
	/// Semantics mirror `VirtualStorage` (ordered keys, `None` end =
	/// unbounded, scan returns FULL keys).
	pub trait Engine: Send + Sync {
	    fn put(&self, key: Vec<u8>, value: Vec<u8>);
	    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
	    fn del(&self, key: &[u8]);
	    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>>;
	}

	/// The collection's engine: the embedded mode's in-process store or
	/// an injected host engine. One executor behind both faces (the
	/// 0037 §1 rule): same `DynamicCollection` code path, the enum only
	/// dispatches bytes.
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

	/// A Python-owned dynamic collection: the embedded-mode facade (ADR-0022).
	/// Python holds the engine (in-process TestStore), registers host
	/// callables at binding time, and drives put/get/delete/scan — the
	/// calling discipline (fold/unfold/seed) runs inside the Rust put path.
	/// The injected face (`Collection::with_store`) swaps ONLY the engine:
	/// a host-supplied `Engine` parks the table in the realm's real store
	/// while Python still writes nothing but Collection calls (zero
	/// translation, ADR-0037 §1).
	#[pyclass]
	pub struct Collection {
	    inner: Arc<Mutex<okm_dynamic::DynamicCollection<EngineBox>>>,
	    schema: okm_core::schema::CollectionSchema,
	    ns: u16,
	}


impl Collection {
    /// The host-injected face (ADR-0037 §1, Phase 4.16a): build a
    /// collection over an ENGINE THE HOST OWNS — the realm's live store
    /// behind the byte-level `Engine` face, zero translation. Lives in a
    /// PLAIN Rust impl (not `#[pymethods]`): Python never constructs this
    /// shape — the carrier's load step calls it per declared collection
    /// and registers the instance into the module namespace — and its
    /// host-side argument types (`Arc<dyn Engine>`, `&serde_json::Value`)
    /// are not marshalable by pyo3. Construction needs no GIL: the
    /// pyclass is plain Rust data until a method runs.
    ///
    /// `entry` is the RAW interface_schema storage entry:
    /// `{ "schema": <CollectionSchema>, "indexes": [{name, slot, fields,
    /// includes, kind}], "reduces": [{name, slot, group, kind}] }` (the
    /// bare schema object is also accepted). Declared indexes must be
    /// plain (host callables — func/partial — register through the
    /// embedded-mode add_* surface instead); reduce kinds take the
    /// okm-dynamic preset spellings ("count", {"high_water": f},
    /// {"low_water": f}). ns is NOT in the entry (the DSL's rule: it
    /// binds at construction).
    pub fn with_store(
        engine: Arc<dyn Engine>,
        entry: &serde_json::Value,
        ns: u16,
    ) -> PyResult<Self> {
        let raw = entry.get("schema").cloned().unwrap_or_else(|| entry.clone());
        let schema: okm_core::schema::CollectionSchema = serde_json::from_value(raw)
            .map_err(|e| PyValueError::new_err(format!("bad collection schema: {e}")))?;
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
        Ok(Collection {
            inner: Arc::new(Mutex::new(okm_dynamic::DynamicCollection::with_reduces(
                EngineBox::Injected(engine),
                ns,
                schema.clone(),
                indexes,
                reduces,
            ))),
            schema,
            ns,
        })
    }

    /// The PLAIN Rust method face (host-side callers: the carrier's
    /// injection checks, embedded harnesses). Same executor, same
    /// `DynamicCollection` calls the #[pymethods] shell makes — the
    /// python shell adds only dict <-> ValueMap coercion on top.
    pub fn put_doc(&self, pkey: &[u8], document: &ValueMap) -> Result<(), String> {
        self.inner.lock().unwrap().put(pkey, document).map_err(|e| e.to_string())
    }

    pub fn get_doc(&self, pkey: &[u8]) -> Result<Option<ValueMap>, String> {
        self.inner.lock().unwrap().get(pkey).map_err(|e| e.to_string())
    }

    pub fn delete_doc(&self, pkey: &[u8]) -> Result<(), String> {
        self.inner.lock().unwrap().delete(pkey).map_err(|e| e.to_string())
    }

    /// Declared-index scan by slot + encoded prefix → decoded keys.
    pub fn scan_doc(&self, slot: u16, encoded_prefix: &[u8]) -> Result<Vec<ValueMap>, String> {
        self.inner.lock().unwrap().scan(slot, encoded_prefix).map_err(|e| e.to_string())
    }
}

#[pymethods]
impl Collection {
	    /// Open a table on a fresh in-process store (embedded mode: Python
	    /// is the single writer by construction).
	    #[new]
	    fn new(schema: &Schema, ns: u16) -> PyResult<Self> {
	        Ok(Collection {
	            inner: Arc::new(Mutex::new(okm_dynamic::DynamicCollection::new(
	                EngineBox::Test(okm_core::TestStore::slatedb_mem()),
	                ns,
	                schema.inner.as_ref().clone(),
	                Vec::new(),
	            ))),
	            schema: schema.inner.as_ref().clone(),
	            ns,
	        })
	    }

	    /// Register a function index (ADR-0022): `func(document) ->
	    /// list[bytes]` produces one entry per derived value (multi-entry
	    /// fan-out, the inverted-index regime). The slot must be unique
	    /// within the table and outside the plain-index range used so far.
	    #[pyo3(signature = (slot, func, includes=None))]
	    fn add_func_index(
	        &self,
	        slot: u16,
	        func: Py<PyAny>,
	        includes: Option<Vec<String>>,
	    ) -> PyResult<()> {
	        let derive = move |document: &ValueMap| -> Result<Vec<Vec<u8>>, String> {
	            Python::with_gil(|py| {
	                let doc = map_to_dict(py, document).map_err(call_err)?;
	                let raw = func
	                    .call(py, (doc,), None)
	                    .map_err(|e| py_to_string(py, e))?;
	                raw.extract::<Vec<Vec<u8>>>(py)
	                    .map_err(|e| py_to_string(py, e))
	            })
	        };
	        self.inner
	            .lock()
	            .unwrap()
	            .declare_index(AccessMethod {
	                slot,
	                fields: vec![], // the derive result IS the segment
	                includes: includes.unwrap_or_default(),
	                kind: AccessMethodKind::Func(Box::new(derive)),
	            })
	            .map_err(PyValueError::new_err)
	    }

	    /// Register a partial index: `admits(document) -> bool` gates whether
	    /// the declared fields produce an entry (Postgres-style). Purity is
	    /// the caller's contract (an impure predicate leaves dangling entries).
	    #[pyo3(signature = (slot, fields, admits, includes=None))]
	    fn add_partial_index(
	        &self,
	        slot: u16,
	        fields: Vec<String>,
	        admits: Py<PyAny>,
	        includes: Option<Vec<String>>,
	    ) -> PyResult<()> {
	        let predicate = move |document: &ValueMap| -> Result<bool, String> {
	            Python::with_gil(|py| {
	                let doc = map_to_dict(py, document).map_err(call_err)?;
	                let raw = admits
	                    .call(py, (doc,), None)
	                    .map_err(|e| py_to_string(py, e))?;
	                raw.extract::<bool>(py)
	                    .map_err(|e| py_to_string(py, e))
	            })
	        };
	        self.inner
	            .lock()
	            .unwrap()
	            .declare_index(AccessMethod {
	                slot,
	                fields,
	                includes: includes.unwrap_or_default(),
	                kind: AccessMethodKind::Partial(Box::new(predicate)),
	            })
	            .map_err(PyValueError::new_err)
	    }

	    /// Register a reduce group: a `ReduceLogic` subclass instance whose
	    /// seed/fold/unfold implement the accumulator semantics (the mirror
	    /// of the Rust-side `ReduceLogic` + `ReduceCodec: Default` pair).
	    fn add_reduce(&self, slot: u16, group_fields: Vec<String>, logic: Py<PyAny>) -> PyResult<()> {
	        let logic = PyReduce { obj: logic };
	        self.inner
	            .lock()
	            .unwrap()
	            .declare_reduce(okm_dynamic::ReduceSpec {
	                slot,
	                group_fields,
	                logic: Box::new(logic),
	            })
	            .map_err(PyValueError::new_err)
	    }

	    /// Write one document: `{field: value}` — key + payload fields in
	    /// one dict (the schema splits them). Runs the index entry lifecycle
	    /// and the reduce calling discipline.
	    fn put(&self, pkey: Vec<u8>, document: &Bound<'_, PyAny>) -> PyResult<()> {
	        // Schema-coerced (Python ints carry no width; the schema kind
	        // drives the coercion — same rule as the codec surface).
	        let map = py_values_to_map(&self.schema, document)?;
	        self.inner
	            .lock()
	            .unwrap()
	            .put(&pkey, &map)
	            .map_err(PyValueError::new_err)
	    }

	    /// Point read by primary key → dict or None.
	    fn get(&self, pkey: Vec<u8>) -> PyResult<Option<PyObject>> {
	        let doc = self
	            .inner
	            .lock()
	            .unwrap()
	            .get(&pkey)
	            .map_err(PyValueError::new_err)?;
	        match doc {
	            Some(m) => Python::with_gil(|py| Ok(Some(map_to_dict(py, &m)?.unbind().into_any()))),
	            None => Ok(None),
	        }
	    }

	    /// Delete a document (index entries + unfold).
	    fn delete(&self, pkey: Vec<u8>) -> PyResult<()> {
	        self.inner
	            .lock()
	            .unwrap()
	            .delete(&pkey)
	            .map_err(PyValueError::new_err)
	    }

	    /// Access-method scan: return the matching documents' primary keys,
	    /// decoded as dicts (dynamic counterpart of `Collection::scan_index`).
	    fn scan(&self, slot: u16, encoded_prefix: Vec<u8>) -> PyResult<Vec<PyObject>> {
	        let docs = self
	            .inner
	            .lock()
	            .unwrap()
	            .scan(slot, &encoded_prefix)
	            .map_err(PyValueError::new_err)?;
	        Python::with_gil(|py| {
	            docs.iter()
	                .map(|m| Ok(map_to_dict(py, m)?.unbind().into_any()))
	                .collect()
	        })
	    }

	    /// Read one reduce group's accumulator bytes (None = no group yet).
	    /// The layout is the host's contract — decode with the same rule the
	    /// `ReduceLogic` class writes.
	    fn reduce_get(&self, group: &Bound<'_, PyAny>) -> PyResult<Option<Vec<u8>>> {
	        let map = py_values_to_map(&self.schema, group)?;
	        let t = self.inner.lock().unwrap();
	        let ek = t
	            .reduce_entry_key(self.ns, &map)
	            .map_err(PyValueError::new_err)?;
	        Ok(t.store().get(&ek))
	    }
	    /// Scan every group of one reduce: list of (group segment bytes,
	    /// acc bytes). Group fields decode with the schema's BE rule.
	    fn scan_reduces(&self, slot: u16) -> PyResult<Vec<(Vec<u8>, Vec<u8>)>> {
	        let t = self.inner.lock().unwrap();
	        Ok(okm_dynamic::scan_reduces(
	            t.store(),
	            &self.ns.to_be_bytes(),
	            slot,
	        ))
	    }

	    // ---- remote mode (ADR-0022): plan surfaces, Python wraps only ----

	    /// Plan one put WITHOUT touching a local engine: returns
	    /// `(frame bytes, [(group_key, new_acc)])`. The frame is the wire
	    /// write frame (one `commit_batch` — document + index entries + acc
	    /// updates atomically); `new_accs` is the receipt the actor adopts
	    /// into its acc cache. `old` is the caller-held stored document
	    /// (None = fresh key); `accs` maps group entry key → current acc
	    /// bytes (the actor IS the authoritative acc holder,
	    /// single-writer-per-group).
	    #[pyo3(signature = (pkey, document, old=None, accs=None))]
	    fn plan_put(
	        &self,
	        pkey: Vec<u8>,
	        document: &Bound<'_, PyAny>,
	        old: Option<&Bound<'_, PyAny>>,
	        accs: Option<&Bound<'_, PyAny>>,
	    ) -> PyResult<(Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>)> {
	        let map = py_values_to_map(&self.schema, document)?;
	        let old = match old {
	            Some(d) => Some(py_values_to_map(&self.schema, d)?),
	            None => None,
	        };
	        let accs = accs_dict(accs)?;
	        let t = self.inner.lock().unwrap();
	        let plan = t
	            .plan_put(&pkey, &map, old.as_ref(), &|ek| {
	                accs.as_ref().and_then(|a| a.get(ek).cloned())
	            })
	            .map_err(PyValueError::new_err)?;
	        let frame = okm_wire::OpFrame::write_batch(&plan.ops).encode();
	        Ok((frame, plan.new_accs))
	    }

	    /// Plan one delete: `(frame bytes, new_accs)` — same contract as
	    /// `plan_put`; an absent `old` means there is nothing to remove
	    /// (empty frame).
	    #[pyo3(signature = (pkey, old, accs=None))]
	    fn plan_delete(
	        &self,
	        pkey: Vec<u8>,
	        old: &Bound<'_, PyAny>,
	        accs: Option<&Bound<'_, PyAny>>,
	    ) -> PyResult<(Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>)> {
	        let old = py_values_to_map(&self.schema, old)?;
	        let accs = accs_dict(accs)?;
	        let t = self.inner.lock().unwrap();
	        let plan = t
	            .plan_delete(&pkey, &old, &|ek| {
	                accs.as_ref().and_then(|a| a.get(ek).cloned())
	            })
	            .map_err(PyValueError::new_err)?;
	        let frame = okm_wire::OpFrame::write_batch(&plan.ops).encode();
	        Ok((frame, plan.new_accs))
	    }

	    /// Decode a stored payload (the receiver's GET answer) into the
	    /// `old` document dict for the next `plan_put` — the cache-refill
	    /// helper the embedded path does against its own engine.
	    fn decode_stored(&self, payload: Vec<u8>) -> PyResult<PyObject> {
	        let m = okm_dynamic::decode_stored(&self.schema, &payload)
	            .map_err(PyValueError::new_err)?;
	        Python::with_gil(|py| Ok(map_to_dict(py, &m)?.unbind().into_any()))
	    }
	}

	/// One declared plain index from the schema-data spelling (the entry
	/// shape okm_schema.py assembles): routing is by slot; kind must be
	/// plain — a host-callable index cannot be built from data (the
	/// embedded add_* surface owns callables).
	fn plain_access_method(v: &serde_json::Value) -> PyResult<AccessMethod> {
	    let obj = v
	        .as_object()
	        .ok_or_else(|| PyValueError::new_err("index entry must be an object"))?;
	    let missing = |k: &str| PyValueError::new_err(format!("index missing {k}"));
	    let slot = obj
	        .get("slot")
	        .and_then(|x| x.as_u64())
	        .ok_or_else(|| missing("slot"))? as u16;
	    let strings = |arr: &serde_json::Value| -> Vec<String> {
	        arr.as_array()
	            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
	            .unwrap_or_default()
	    };
	    let fields = obj.get("fields").map(strings).ok_or_else(|| missing("fields"))?;
	    let includes = obj.get("includes").map(strings).unwrap_or_default();
	    let kind = obj.get("kind").and_then(|x| x.as_str()).unwrap_or("plain");
	    if kind != "plain" {
	        return Err(PyValueError::new_err(format!(
	            "index slot {slot}: kind `{kind}` needs a host callable — use the embedded add_* surface"
	        )));
	    }
	    Ok(AccessMethod { slot, fields, includes, kind: AccessMethodKind::Plain })
	}

	/// One declared preset reduce from the schema-data spelling:
	/// `{ "slot": N, "group": [fields], "kind": "count" |
	/// {"high_water": f} | {"low_water": f} }` — the okm-dynamic presets
	/// (the u64 BE accumulator shared with the derive and the aura
	/// executor: one implementation, every host).
	fn preset_reduce(v: &serde_json::Value) -> PyResult<okm_dynamic::ReduceSpec> {
	    let obj = v
	        .as_object()
	        .ok_or_else(|| PyValueError::new_err("reduce entry must be an object"))?;
	    let slot = obj
	        .get("slot")
	        .and_then(|x| x.as_u64())
	        .ok_or_else(|| PyValueError::new_err("reduce missing slot"))? as u16;
	    let group_fields = obj
	        .get("group")
	        .and_then(|x| x.as_array())
	        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
	        .unwrap_or_default();
	    let kind_v = obj
	        .get("kind")
	        .ok_or_else(|| PyValueError::new_err("reduce missing kind"))?;
	    let kind = if let Some(s) = kind_v.as_str() {
	        match s {
	            "count" => okm_dynamic::PresetKind::Count,
	            other => return Err(PyValueError::new_err(format!("unknown reduce kind `{other}`"))),
	        }
	    } else if let Some(o) = kind_v.as_object().filter(|o| o.len() == 1) {
	        let (k, f) = o.iter().next().unwrap();
	        let field = f
	            .as_str()
	            .ok_or_else(|| PyValueError::new_err("reduce kind field must be a string"))?
	            .to_string();
	        match k.as_str() {
	            "high_water" => okm_dynamic::PresetKind::HighWater(field),
	            "low_water" => okm_dynamic::PresetKind::LowWater(field),
	            other => return Err(PyValueError::new_err(format!("unknown reduce kind `{other}`"))),
	        }
	    } else {
	        return Err(PyValueError::new_err("reduce kind must be a string or {kind: field}"));
	    };
	    Ok(okm_dynamic::ReduceSpec {
	        slot,
	        group_fields,
	        logic: Box::new(okm_dynamic::PresetLogic { kind }),
	    })
	}

	/// `{group_key bytes: acc bytes}` → Rust lookup (None = no dict given).
	fn accs_dict(accs: Option<&Bound<'_, PyAny>>) -> PyResult<Option<std::collections::HashMap<Vec<u8>, Vec<u8>>>> {
	    let Some(a) = accs else { return Ok(None) };
	    let d = a
	        .downcast::<pyo3::types::PyDict>()
	        .map_err(|_| PyTypeError::new_err("accs must be a dict {group_key bytes: acc bytes}"))?;
	    let mut m = std::collections::HashMap::new();
	    for (k, v) in d.iter() {
	        let key: Vec<u8> = k.extract()?;
	        let val: Vec<u8> = v.extract()?;
	        m.insert(key, val);
	    }
	    Ok(Some(m))
	}

	/// A parsed OKM table schema (re-exported from the codec module so the
	/// Collection constructor takes the same object the codec surface returns).
	#[pyclass]
	struct Schema {
	    inner: Arc<okm_core::schema::CollectionSchema>,
	}

	#[pymethods]
	impl Schema {
	    /// Build from `Collection::json_schema()` output or any serde'd
	    /// CollectionSchema JSON.
	    #[new]
	    fn from_json(json: &str) -> PyResult<Self> {
	        let schema: okm_core::schema::CollectionSchema = serde_json::from_str(json)
	            .map_err(|e| PyValueError::new_err(format!("bad schema json: {e}")))?;
	        Ok(Schema { inner: Arc::new(schema) })
	    }

	    /// Layout version this schema declares.
	    #[getter]
	    fn layout_version(&self) -> u8 {
	        self.inner.layout_version
	    }

	    /// Encode key fields from `{field: value}` (subset order-independent;
	    /// missing key fields are a ValueError).
	    fn encode_key(&self, values: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
	        let map = py_values_to_map(&self.inner, values)?;
	        okm_dynamic::encode_key(&self.inner, &map).map_err(codec_err)
	    }

	    /// Encode the payload (hot segment + cold TLV) from `{field: value}`.
	    fn encode_payload(&self, values: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
	        let map = py_values_to_map(&self.inner, values)?;
	        okm_dynamic::encode_payload(&self.inner, &map).map_err(codec_err)
	    }

	    /// Decode key bytes → `{field: value}`.
	    fn decode_key(&self, bytes: &[u8]) -> PyResult<PyObject> {
	        okm_dynamic::decode_key(&self.inner, bytes)
	            .map(|m| Python::with_gil(|py| map_to_dict(py, &m).unwrap().unbind().into_any()))
	            .map_err(codec_err)
	    }

	    /// Decode payload bytes → `{field: value}`. Absent tail fields arrive
	    /// as their schema defaults (version migration).
	    fn decode_payload(&self, bytes: &[u8]) -> PyResult<PyObject> {
	        okm_dynamic::decode_payload(&self.inner, bytes)
	            .map(|m| Python::with_gil(|py| map_to_dict(py, &m).unwrap().unbind().into_any()))
	            .map_err(codec_err)
	    }
	}

	/// Schema-coerced dict → ValueMap (ints carry no Python width; the
	/// schema kind drives the coercion).
	fn py_values_to_map(
	    schema: &okm_core::schema::CollectionSchema,
	    values: &Bound<'_, PyAny>,
	) -> PyResult<ValueMap> {
	    let dict = values
	        .downcast::<pyo3::types::PyDict>()
	        .map_err(|_| PyTypeError::new_err("values must be a dict {field: value}"))?;
	    let mut out = ValueMap::new();
	    for (k, v) in dict.iter() {
	        let name: String = k.extract()?;
	        let f = schema
	            .key_fields
	            .iter()
	            .chain(schema.hot_fields.iter())
	            .chain(schema.cold_fields.iter())
	            .find(|f| f.name == name)
	            .ok_or_else(|| {
	                PyValueError::new_err(format!("field `{name}`: not declared in the schema"))
	            })?;
	        out.insert(name.clone(), coerce(&name, f.ty, &v)?);
	    }
	    Ok(out)
	}

	/// Python has no integer-width distinction; coerce by the field's schema
	/// kind so `{"level": 9}` fills a U32 field, a U64 field, etc.
	fn coerce(name: &str, kind: okm_core::FieldType, v: &Bound<'_, PyAny>) -> PyResult<Value> {
	    use okm_core::FieldType;
	    let bad = |expected: &str| {
	        PyTypeError::new_err(format!("field `{name}`: expected {expected}"))
	    };
	    Ok(match kind {
	        FieldType::U8 => Value::U8(v.extract::<u64>().map_err(|_| bad("u8"))? as u8),
	        FieldType::U16 => Value::U16(v.extract::<u64>().map_err(|_| bad("u16"))? as u16),
	        FieldType::U32 => Value::U32(v.extract::<u64>().map_err(|_| bad("u32"))? as u32),
	        FieldType::U64 | FieldType::VarInt | FieldType::Quant(_) | FieldType::Offset(_) => {
	            Value::U64(v.extract::<u64>().map_err(|_| bad("u64"))?)
	        }
	        FieldType::Enum => Value::U8(v.extract::<u64>().map_err(|_| bad("enum tag u8"))? as u8),
	        FieldType::FixedBytes | FieldType::Bytes => {
	            Value::Bytes(v.extract::<Vec<u8>>().map_err(|_| bad("bytes"))?)
	        }
	        FieldType::Str => Value::Str(v.extract::<String>().map_err(|_| bad("str"))?),
	        FieldType::Vector { .. } => {
	            Value::Bytes(v.extract::<Vec<u8>>().map_err(|_| bad("flat LE vector bytes"))?)
	        }
	    })
	}

	fn codec_err(e: okm_dynamic::CodecError) -> PyErr {
	    PyValueError::new_err(format!("{e}"))
	}

	/// okm — OKM dynamic codec + embedded-mode table for Python.
	#[pymodule]
	fn okm(m: &Bound<'_, PyModule>) -> PyResult<()> {
	    m.add_class::<Schema>()?;
	    m.add_class::<Collection>()?;
	    Ok(())
	}

}

/// The python schema-declaration DSL as an embeddable string (ADR-0026
/// §4): hosts (probe's python carrier, aura) inject this module into
/// actor scripts so `@KeyEncode` / `@DocumentEncode` / `@ok_*` resolve
/// and `assemble_module` produces the storage block — one source, every
/// consumer embedding the same module.
pub const OKM_SCHEMA_PY: &str = include_str!("../okm/okm_schema.py");

