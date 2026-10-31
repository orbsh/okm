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
    use pyo3::Py;
    use std::sync::{Arc, Mutex};

    /// Convert a ValueMap (decoded document) into a Python dict.
    fn map_to_dict<'py>(
        py: Python<'py>,
        m: &ValueMap,
    ) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let d = pyo3::types::PyDict::new(py);
        for (k, v) in m {
            let value: pyo3::Py<PyAny> = match v {
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
    fn value_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Py<PyAny>> {
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
                    .ok_or_else(|| {
                        pyo3::exceptions::PyRuntimeError::new_err("value roundtrip failed")
                    })?
                    .unbind()
            }
        })
    }

    /// The value types a path-index entry can encode (ADR-0029): the fixed
    /// unsigned widths, `str`, and fixed-width bytes. Everything else is refused
    /// at declaration — a face that can never produce an entry answers nothing,
    /// silently, forever.
    fn path_value_type(name: &str) -> PyResult<okm_core::field::FieldType> {
        Ok(match name {
            "u8" => okm_core::field::FieldType::U8,
            "u16" => okm_core::field::FieldType::U16,
            "u32" => okm_core::field::FieldType::U32,
            "u64" => okm_core::field::FieldType::U64,
            "str" => okm_core::field::FieldType::Str,
            "fixed" => okm_core::field::FieldType::FixedBytes,
            other => {
                return Err(PyValueError::new_err(format!(
                    "path index value type must be u8/u16/u32/u64/str/fixed, got {other}"
                )));
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
            Python::attach(|py| {
                self.obj
                    .call_method(py, "seed", (), None)
                    .and_then(|b| b.extract::<Vec<u8>>(py))
                    .unwrap_or_default()
            })
        }

        fn fold(
            &self,
            acc: &mut Vec<u8>,
            key: &ValueMap,
            document: &ValueMap,
        ) -> Result<(), String> {
            Python::attach(|py| self.apply(py, "fold", acc, key, document))
        }

        fn unfold(
            &self,
            acc: &mut Vec<u8>,
            key: &ValueMap,
            document: &ValueMap,
        ) -> Result<(), String> {
            Python::attach(|py| self.apply(py, "unfold", acc, key, document))
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

    /// The injected engine face + the one-executor enum (ADR-0037 4.16a)
    /// RE-EXPORTED from `okm-entry` — the single source shared with
    /// okm-steel's Collection face (4.16b); a binding re-implementing
    /// it is the layout-fork the user rejected.
    pub use okm_entry::{Engine, EngineBox};

    /// The plan receipts the remote face ships: (frame bytes, [(group key,
    /// new acc)]) — named once for the two plan methods' signatures.
    type PlanReceipt = (Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>);

    /// A table's meta record (ADR-0043) → `{name: bytes}`: the one shape all
    /// three verbs answer with, so `ensure_layout`, `meta` and `set_meta`
    /// cannot drift apart.
    fn meta_to_py(py: Python<'_>, meta: &okm_core::TableMeta) -> PyResult<Py<PyAny>> {
        let dict = pyo3::types::PyDict::new(py);
        for (name, value) in meta.iter() {
            dict.set_item(name, value)?;
        }
        Ok(dict.unbind().into_any())
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
            // The entry parsing (schema + declared plain indexes + preset
            // reduces) lives in `okm-entry` — the single source shared with
            // okm-steel (4.16b); a binding-side copy is the layout fork the
            // user rejected.
            let (schema, table) =
                okm_entry::collection_from_entry(EngineBox::Injected(engine), entry, ns)
                    .map_err(PyValueError::new_err)?;
            Ok(Collection {
                inner: Arc::new(Mutex::new(table)),
                schema,
                ns,
            })
        }

        /// The PLAIN Rust method face (host-side callers: the carrier's
        /// injection checks, embedded harnesses). Same executor, same
        /// `DynamicCollection` calls the #[pymethods] shell makes — the
        /// python shell adds only dict <-> ValueMap coercion on top.
        pub fn merge_doc(&self, pkey: &[u8], document: &ValueMap) -> Result<(), String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .merge(pkey, document)
                .map_err(|e| e.to_string())
        }

        pub fn fetch_doc(&self, pkey: &[u8]) -> Result<Option<ValueMap>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fetch(pkey)
                .map_err(|e| e.to_string())
        }

        pub fn purge_doc(&self, pkey: &[u8]) -> Result<bool, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .purge(pkey)
                .map_err(|e| e.to_string())
        }

        /// Declared-index scan by slot + encoded prefix → decoded keys.
        pub fn scan_doc(&self, slot: u16, encoded_prefix: &[u8]) -> Result<Vec<ValueMap>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .scan(slot, encoded_prefix)
                .map_err(|e| e.to_string())
        }

        /// Table meta (ADR-0043): stamp the store with this schema's layout
        /// version, or answer the stamp it already carries. Same row the typed
        /// path writes — one format, two modes.
        pub fn ensure_layout_doc(&self) -> Result<okm_core::TableMeta, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ensure_layout()
        }

        /// The store's table record as it stands (ADR-0043), `None` when the
        /// store was never stamped. A READ — stamping is `ensure_layout`.
        pub fn meta_doc(&self) -> Result<Option<okm_core::TableMeta>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .meta()
        }

        /// Record one caller-owned name beside the runtime's stamp (ADR-0043
        /// §5), answering the whole record. The executor re-stamps `version`
        /// after the caller's name lands, so this verb cannot edit the shape
        /// away.
        pub fn set_meta_doc(
            &self,
            name: &str,
            value: &[u8],
        ) -> Result<okm_core::TableMeta, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_meta(name, value.to_vec())
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
        /// `version` is the caller's claim that the DERIVE changed: a closure has no
        /// hashable identity, so the fingerprint cannot see a new function at an
        /// unchanged slot. Bump it and the table answers `Stale`; the rebuild
        /// re-derives every entry. `0` means no claim.
        #[pyo3(signature = (slot, func, includes=None, version=0))]
        fn add_func_index(
            &self,
            slot: u16,
            func: Py<PyAny>,
            includes: Option<Vec<String>>,
            version: u32,
        ) -> PyResult<()> {
            let derive = move |document: &ValueMap| -> Result<Vec<Vec<u8>>, String> {
                Python::attach(|py| {
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
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .declare_index(AccessMethod {
                    slot,
                    fields: vec![], // the derive result IS the segment
                    includes: includes.unwrap_or_default(),
                    kind: AccessMethodKind::Func(Box::new(derive)),
                    version,
                })
                .map_err(PyValueError::new_err)
        }

        /// Register a partial index: `admits(document) -> bool` gates whether
        /// the declared fields produce an entry (Postgres-style). Purity is
        /// the caller's contract (an impure predicate leaves dangling entries).
        /// `version`: the same claim as `add_func_index` — a predicate change alters
        /// which entries exist and cannot be hashed.
        #[pyo3(signature = (slot, fields, admits, includes=None, version=0))]
        fn add_partial_index(
            &self,
            slot: u16,
            fields: Vec<String>,
            admits: Py<PyAny>,
            includes: Option<Vec<String>>,
            version: u32,
        ) -> PyResult<()> {
            let predicate = move |document: &ValueMap| -> Result<bool, String> {
                Python::attach(|py| {
                    let doc = map_to_dict(py, document).map_err(call_err)?;
                    let raw = admits
                        .call(py, (doc,), None)
                        .map_err(|e| py_to_string(py, e))?;
                    raw.extract::<bool>(py).map_err(|e| py_to_string(py, e))
                })
            };
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .declare_index(AccessMethod {
                    slot,
                    fields,
                    includes: includes.unwrap_or_default(),
                    kind: AccessMethodKind::Partial(Box::new(predicate)),
                    version,
                })
                .map_err(PyValueError::new_err)
        }

        /// Register a path index (ADR-0029): `path` resolves inside the
        /// dynamic segment, one entry per resolved value (fan-out over
        /// arrays), encoded as `value_ty`. The slot is the path's identity —
        /// the entry key does not carry the path, so the same slot may not be
        /// declared twice.
        ///
        /// `value_ty` is one of `u8` / `u16` / `u32` / `u64` / `str` /
        /// `fixed`: the types the entry encoder can produce. Anything else is
        /// refused here rather than declared as a face that never answers.
        #[pyo3(signature = (slot, path, value_ty = "str", version = 0))]
        fn add_path_index(
            &self,
            slot: u16,
            path: &str,
            value_ty: &str,
            version: u32,
        ) -> PyResult<()> {
            let path = okm_dynamic::Path::parse(path).map_err(PyValueError::new_err)?;
            let value_ty = path_value_type(value_ty)?;
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .declare_index(AccessMethod {
                    slot,
                    // The path IS the source; `fields` stays empty (ADR-0029).
                    fields: vec![],
                    includes: vec![],
                    kind: AccessMethodKind::Path { path, value_ty },
                    version,
                })
                .map_err(PyValueError::new_err)
        }

        /// Register a reduce group: a `ReduceLogic` subclass instance whose
        /// seed/fold/unfold implement the accumulator semantics (the mirror
        /// of the Rust-side `ReduceLogic` + `ReduceCodec: Default` pair).
        fn add_reduce(
            &self,
            slot: u16,
            group_fields: Vec<String>,
            logic: Py<PyAny>,
        ) -> PyResult<()> {
            let logic = PyReduce { obj: logic };
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .declare_reduce(okm_dynamic::ReduceSpec {
                    slot,
                    group_fields,
                    logic: Box::new(logic),
                })
                .map_err(PyValueError::new_err)
        }

        /// Whether the store's faces were built for THIS declaration (ADR-0048):
        /// `{"state": "match" | "stale" | "unknown", "current": bytes,
        /// "stored": bytes | None}`. A host that declares its methods at runtime
        /// changes the declaration without a compiler noticing, so it needs a way to
        /// ask — this is it.
        fn faces_status(&self) -> PyResult<Py<PyAny>> {
            use okm_core::FaceStatus;
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let status = guard.faces_status().map_err(PyValueError::new_err)?;
            // `Match` carries no bytes of its own — the stored stamp IS the number,
            // so read it once and report it as both sides.
            let header = self.ns.to_be_bytes();
            let stamped = okm_core::meta::read(guard.store(), &header)
                .map_err(|e| PyValueError::new_err(e.to_string()))?
                .and_then(|m| m.faces());
            Python::attach(|py| {
                let d = pyo3::types::PyDict::new(py);
                let (state, current, stored) = match status {
                    FaceStatus::Match => ("match", stamped.unwrap_or_default(), stamped),
                    FaceStatus::Stale { stored, current } => ("stale", current, Some(stored)),
                    FaceStatus::Unknown { current } => ("unknown", current, None),
                };
                d.set_item("state", state)?;
                d.set_item("current", current.to_vec())?;
                d.set_item("stored", stored.map(|f| f.to_vec()))?;
                Ok(d.into_any().unbind())
            })
        }

        /// Rebuild every face this table declares, then stamp (ADR-0048): the
        /// repair for a `stale` answer. `chunk` bounds ops per commit — the default
        /// is one commit, which is atomic but holds the whole face set in memory.
        #[pyo3(signature = (chunk=None))]
        fn rebuild_faces(&self, chunk: Option<usize>) -> PyResult<Py<PyAny>> {
            let report = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .rebuild_faces(chunk.unwrap_or(usize::MAX))
                .map_err(PyValueError::new_err)?;
            Python::attach(|py| {
                let d = pyo3::types::PyDict::new(py);
                d.set_item("faces_swept", report.faces_swept)?;
                d.set_item("entries_deleted", report.entries_deleted)?;
                d.set_item("documents", report.documents)?;
                d.set_item("entries_written", report.entries_written)?;
                d.set_item("chunks", report.chunks)?;
                Ok(d.into_any().unbind())
            })
        }

        /// Update one document: `{name: value}` — a declared name rewrites that
        /// field of the row, any other name rides the dynamic segment. Runs the
        /// index entry lifecycle and the reduce calling discipline. This is the
        /// write door: there is no `put` here (ADR-0035 — the whole-document
        /// verbs are the surface, and `merge` covers the same op list).
        fn merge(&self, pkey: Vec<u8>, document: &Bound<'_, PyAny>) -> PyResult<()> {
            // Schema-coerced for a DECLARED name (Python ints carry no width; the
            // schema kind drives the coercion — same rule as the codec surface),
            // and typed by the Python value for any other name, which rides the
            // dynamic segment (ADR-0035). A path index (ADR-0029) indexes exactly
            // that segment, so refusing undeclared names here would make
            // `add_path_index` a face nothing can feed.
            let map = py_values_to_map_any(&self.schema, document)?;
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .merge(&pkey, &map)
                .map_err(PyValueError::new_err)
        }

        /// The whole document (both slots) → dict or None: the read door.
        fn fetch(&self, pkey: Vec<u8>) -> PyResult<Option<Py<PyAny>>> {
            let doc = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fetch(&pkey)
                .map_err(PyValueError::new_err)?;
            match doc {
                Some(m) => Python::attach(|py| Ok(Some(map_to_dict(py, &m)?.unbind().into_any()))),
                None => Ok(None),
            }
        }

        /// Remove the whole document — row, faces, ledger, dynamic segment — in
        /// one commit. `True` = there was something to remove.
        fn purge(&self, pkey: Vec<u8>) -> PyResult<bool> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .purge(&pkey)
                .map_err(PyValueError::new_err)
        }

        /// Stamp this table on the store, or answer the stamp it already carries
        /// (ADR-0043): `{name: bytes}`. Raises `ValueError` naming both numbers
        /// when the store was written in another layout version — the shape check
        /// belongs where the host attaches, not on the write path.
        fn ensure_layout(&self) -> PyResult<Py<PyAny>> {
            let meta = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ensure_layout()
                .map_err(PyValueError::new_err)?;
            Python::attach(|py| meta_to_py(py, &meta))
        }

        /// This table's record on the store as it stands (ADR-0043):
        /// `{name: bytes}`, or `None` when nothing has stamped it. A read —
        /// stamping belongs to `ensure_layout`, and a `None` here is a fresh
        /// (or foreign) store, not an error.
        fn meta(&self) -> PyResult<Option<Py<PyAny>>> {
            let meta = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .meta()
                .map_err(PyValueError::new_err)?;
            match meta {
                Some(meta) => Python::attach(|py| meta_to_py(py, &meta)).map(Some),
                None => Ok(None),
            }
        }

        /// Record one caller-owned name beside the runtime's stamp (ADR-0043
        /// §5) and answer the whole record. `version` is re-stamped afterwards,
        /// so this verb cannot edit the shape away; a store stamped in another
        /// version still raises `ValueError` naming both numbers.
        fn set_meta(&self, name: String, value: Vec<u8>) -> PyResult<Py<PyAny>> {
            let meta = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_meta(&name, value)
                .map_err(PyValueError::new_err)?;
            Python::attach(|py| meta_to_py(py, &meta))
        }

        /// Access-method scan: return the matching documents' primary keys,
        /// decoded as dicts (dynamic counterpart of `Collection::scan_index`).
        fn scan(&self, slot: u16, encoded_prefix: Vec<u8>) -> PyResult<Vec<Py<PyAny>>> {
            let docs = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .scan(slot, &encoded_prefix)
                .map_err(PyValueError::new_err)?;
            Python::attach(|py| {
                docs.iter()
                    .map(|m| Ok(map_to_dict(py, m)?.unbind().into_any()))
                    .collect()
            })
        }

        /// Equality scan over one access method: only documents whose indexed
        /// fields are EXACTLY the probe, so the probe must name every field of
        /// the data segment (a partial probe is a group query — use `scan`).
        /// Row-free exact matching: a longer stored value and a probe completed
        /// by the primary-key tail are both excluded (dynamic counterpart of
        /// `Collection::scan_exact`).
        fn scan_exact(&self, slot: u16, encoded_prefix: Vec<u8>) -> PyResult<Vec<Py<PyAny>>> {
            let docs = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .scan_exact(slot, &encoded_prefix)
                .map_err(PyValueError::new_err)?;
            Python::attach(|py| {
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
            let t = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ek = t
                .reduce_entry_key(self.ns, &map)
                .map_err(PyValueError::new_err)?;
            t.store()
                .get(&ek)
                .map_err(|e| PyValueError::new_err(e.to_string()))
        }
        /// Scan every group of one reduce: list of (group segment bytes,
        /// acc bytes). Group fields decode with the schema's BE rule.
        fn scan_reduces(&self, slot: u16) -> PyResult<Vec<(Vec<u8>, Vec<u8>)>> {
            let t = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            okm_dynamic::scan_reduces(t.store(), &self.ns.to_be_bytes(), slot)
                .map_err(PyValueError::new_err)
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
        ) -> PyResult<PlanReceipt> {
            let map = py_values_to_map(&self.schema, document)?;
            let old = match old {
                Some(d) => Some(py_values_to_map(&self.schema, d)?),
                None => None,
            };
            let accs = accs_dict(accs)?;
            let t = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let plan = t
                .plan_put(&pkey, &map, old.as_ref(), &|ek| {
                    Ok(accs.as_ref().and_then(|a| a.get(ek).cloned()))
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
        ) -> PyResult<PlanReceipt> {
            let old = py_values_to_map(&self.schema, old)?;
            let accs = accs_dict(accs)?;
            let t = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let plan = t
                .plan_delete(&pkey, &old, &|ek| {
                    Ok(accs.as_ref().and_then(|a| a.get(ek).cloned()))
                })
                .map_err(PyValueError::new_err)?;
            let frame = okm_wire::OpFrame::write_batch(&plan.ops).encode();
            Ok((frame, plan.new_accs))
        }

        /// Decode a stored payload (the receiver's GET answer) into the
        /// `old` document dict for the next `plan_put` — the cache-refill
        /// helper the embedded path does against its own engine.
        fn decode_stored(&self, payload: Vec<u8>) -> PyResult<Py<PyAny>> {
            let m = okm_dynamic::decode_stored(&self.schema, &payload)
                .map_err(PyValueError::new_err)?;
            Python::attach(|py| Ok(map_to_dict(py, &m)?.unbind().into_any()))
        }
    }

    /// `{group_key bytes: acc bytes}` → Rust lookup (None = no dict given).
    fn accs_dict(
        accs: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Option<std::collections::HashMap<Vec<u8>, Vec<u8>>>> {
        let Some(a) = accs else { return Ok(None) };
        let d = a.cast::<pyo3::types::PyDict>().map_err(|_| {
            PyTypeError::new_err("accs must be a dict {group_key bytes: acc bytes}")
        })?;
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
            Ok(Schema {
                inner: Arc::new(schema),
            })
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
        fn decode_key(&self, bytes: &[u8]) -> PyResult<Py<PyAny>> {
            okm_dynamic::decode_key(&self.inner, bytes)
                .map_err(codec_err)
                .and_then(|m| {
                    Python::attach(|py| map_to_dict(py, &m).map(|d| d.unbind().into_any()))
                })
        }

        /// Decode payload bytes → `{field: value}`. Absent tail fields arrive
        /// as their schema defaults (version migration).
        fn decode_payload(&self, bytes: &[u8]) -> PyResult<Py<PyAny>> {
            okm_dynamic::decode_payload(&self.inner, bytes)
                .map_err(codec_err)
                .and_then(|m| {
                    Python::attach(|py| map_to_dict(py, &m).map(|d| d.unbind().into_any()))
                })
        }
    }

    /// A document dict for a write verb: a DECLARED name is coerced by the schema
    /// (Python ints carry no width), anything else is typed by its Python value and
    /// rides the dynamic segment (ADR-0035).
    fn py_values_to_map_any(
        schema: &okm_core::schema::CollectionSchema,
        values: &Bound<'_, PyAny>,
    ) -> PyResult<ValueMap> {
        let dict = values
            .cast::<pyo3::types::PyDict>()
            .map_err(|_| PyTypeError::new_err("values must be a dict {field: value}"))?;
        let mut out = ValueMap::new();
        for (k, v) in dict.iter() {
            let name: String = k.extract()?;
            let declared = schema
                .key_fields
                .iter()
                .chain(schema.hot_fields.iter())
                .chain(schema.cold_fields.iter())
                .find(|f| f.name == name);
            let value = match declared {
                Some(f) => coerce(&name, f.ty, &v)?,
                None => py_dynamic_value(&name, &v)?,
            };
            out.insert(name, value);
        }
        Ok(out)
    }

    /// Python value → dynamic-segment `Value`, typed by the Python type alone: the
    /// segment carries no width, so an `int` has none either. `bool` is checked
    /// before `int` (Python's `bool` IS an `int`), and a `list` / `dict` becomes the
    /// segment's composite forms (ADR-0012 nTLV / count-prefixed elements).
    fn py_dynamic_value(name: &str, v: &Bound<'_, PyAny>) -> PyResult<Value> {
        use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString};
        if v.is_none() {
            return Ok(Value::Null);
        }
        if v.is_instance_of::<PyBool>() {
            return Ok(Value::Bool(v.extract::<bool>()?));
        }
        if v.is_instance_of::<PyInt>() {
            return Ok(Value::U64(v.extract::<u64>().map_err(|_| {
                PyTypeError::new_err(format!("field `{name}`: negative or out-of-range int"))
            })?));
        }
        if v.is_instance_of::<PyFloat>() {
            return Ok(Value::F64(v.extract::<f64>()?));
        }
        if v.is_instance_of::<PyString>() {
            return Ok(Value::Str(v.extract::<String>()?));
        }
        if v.is_instance_of::<PyBytes>() {
            return Ok(Value::Bytes(v.extract::<Vec<u8>>()?));
        }
        if v.is_instance_of::<PyList>() {
            let list = v.cast::<PyList>()?;
            let mut items = Vec::with_capacity(list.len());
            for item in list.iter() {
                items.push(py_dynamic_value(name, &item)?);
            }
            return Ok(Value::Array(items));
        }
        if v.is_instance_of::<PyDict>() {
            let dict = v.cast::<PyDict>()?;
            let mut obj = ValueMap::new();
            for (k, item) in dict.iter() {
                let key: String = k.extract()?;
                obj.insert(key.clone(), py_dynamic_value(&key, &item)?);
            }
            return Ok(Value::Obj(obj));
        }
        Err(PyTypeError::new_err(format!(
            "field `{name}`: an undeclared name takes str / int / float / bool / bytes / list / dict / None"
        )))
    }

    /// Schema-coerced dict → ValueMap (ints carry no Python width; the
    /// schema kind drives the coercion).
    fn py_values_to_map(
        schema: &okm_core::schema::CollectionSchema,
        values: &Bound<'_, PyAny>,
    ) -> PyResult<ValueMap> {
        let dict = values
            .cast::<pyo3::types::PyDict>()
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
        let bad =
            |expected: &str| PyTypeError::new_err(format!("field `{name}`: expected {expected}"));
        Ok(match kind {
            FieldType::U8 => Value::U8(v.extract::<u64>().map_err(|_| bad("u8"))? as u8),
            FieldType::U16 => Value::U16(v.extract::<u64>().map_err(|_| bad("u16"))? as u16),
            FieldType::U32 => Value::U32(v.extract::<u64>().map_err(|_| bad("u32"))? as u32),
            FieldType::U64 | FieldType::VarInt => {
                Value::U64(v.extract::<u64>().map_err(|_| bad("u64"))?)
            }
            // A wrapper carries its own arithmetic in the schema, so its value is
            // LOGICAL here: `Quant` takes an f64 (the codec quantises with P) and
            // `Offset` an i64 (the codec biases with the base).
            FieldType::Quant(_) => Value::F64(v.extract::<f64>().map_err(|_| bad("f64"))?),
            FieldType::Offset(_) => Value::I64(v.extract::<i64>().map_err(|_| bad("i64"))?),
            FieldType::FixedBytes | FieldType::Bytes => {
                Value::Bytes(v.extract::<Vec<u8>>().map_err(|_| bad("bytes"))?)
            }
            // A uuid crosses as its 16 raw bytes, like any fixed-width binary field.
            FieldType::Uuid4 | FieldType::Uuid7 => {
                Value::Bytes(v.extract::<Vec<u8>>().map_err(|_| bad("16 bytes (uuid)"))?)
            }
            FieldType::Str => Value::Str(v.extract::<String>().map_err(|_| bad("str"))?),
            // ADR-0032: a Vocab field travels as its id on the wire; the read
            // side resolves ids to labels through the vocabulary.
            FieldType::Vocab(_) => {
                Value::U8(v.extract::<u64>().map_err(|_| bad("vocab id (u8)"))? as u8)
            }
            FieldType::Vector { .. } => Value::Bytes(
                v.extract::<Vec<u8>>()
                    .map_err(|_| bad("flat LE vector bytes"))?,
            ),
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
