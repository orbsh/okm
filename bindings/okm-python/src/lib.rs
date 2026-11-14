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
//! - `Graph` — the dynamic graph's SECOND face over the same store
//!   (ADR-0017 §4): `incorporate` / `retract` take fragment dicts, the
//!   cluster reads answer plain values, `out`/`inn`/`walk` hand back a
//!   lazily-driven `EdgeStream`, and `KeySet` keeps okm-query's two-step
//!   semantics (set algebra first, one fetch after) instead of degenerating
//!   into loops over ids. Its layout knowledge stays in okm-dynamic: this
//!   file drives the cursor and never re-derives a key shape.
//!
//! Errors cross as Python ValueError (dynamic-side discipline: errors
//! are ordinary input).
pub mod pyo3_impl {
    use okm_core::storage::VirtualStorage;
    use okm_core::{Direction, NodeRef};
    use okm_dynamic::{
        AccessMethod, AccessMethodKind, Cluster, ClusterRef, Decl, Dict, DynEdge, DynFragment,
        EdgeStep, Kind, ReduceLogic, RetractReport, Value, WriteReport,
    };
    use okm_dynamic::Graph as DynGraph;
    use pyo3::exceptions::{PyTypeError, PyValueError};
    use pyo3::prelude::*;
    use pyo3::Py;
    use std::sync::{Arc, Mutex};

    /// Convert a Dict (decoded document) into a Python dict.
    fn map_to_dict<'py>(
        py: Python<'py>,
        m: &Dict,
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
                // Nested composites (okm 0c2a354): Map → dict, Array → list.
                Value::Map(fields) => value_to_py(py, &Value::Map(fields.clone()))?,
                Value::Array(items) => value_to_py(py, &Value::Array(items.clone()))?,
            };
            d.set_item(k, value)?;
        }
        Ok(d)
    }

    /// Value → python object (the composite arm map_to_dict needs: Map
    /// recurses through a dict, Array maps element-wise).
    fn value_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Py<PyAny>> {
        Ok(match v {
            Value::Map(fields) => {
                let inner: Dict = fields.clone();
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
                let one = Dict::from([("__v__".to_string(), other.clone())]);
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
            key: &Dict,
            document: &Dict,
        ) -> Result<(), String> {
            Python::attach(|py| self.apply(py, "fold", acc, key, document))
        }

        fn unfold(
            &self,
            acc: &mut Vec<u8>,
            key: &Dict,
            document: &Dict,
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
            key: &Dict,
            document: &Dict,
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
        /// python shell adds only dict <-> Dict coercion on top.
        /// `replace = false` merges the names the map carries; `true` makes the
        /// map the whole document (a name it omits is gone). One verb either
        /// way — the payload type is what selects the path.
        pub fn put_doc(
            &self,
            pkey: &[u8],
            document: &Dict,
            replace: bool,
        ) -> Result<(), String> {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if replace {
                inner.put(pkey, (document,))
            } else {
                inner.put(pkey, document)
            }
            .map_err(|e| e.to_string())
        }

        pub fn fetch_doc(&self, pkey: &[u8]) -> Result<Option<Dict>, String> {
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
        pub fn scan_doc(&self, slot: u16, encoded_prefix: &[u8]) -> Result<Vec<Dict>, String> {
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

        /// This collection's engine as a HANDLE — a clone is the keyspace, not a
        /// copy, so a second face over it (a `Graph`, a traversal cursor, a key
        /// set's `fetch`) reads the rows this collection wrote. The dynamic form
        /// is schema-as-data, which is what makes one store carry both.
        pub fn store_handle(&self) -> EngineBox {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .store()
                .clone()
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
                    EngineBox::Test(okm_async::TestStore::slatedb_mem()),
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
            let derive = move |document: &Dict| -> Result<Vec<Vec<u8>>, String> {
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
            let predicate = move |document: &Dict| -> Result<bool, String> {
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

        /// Write one document: `{name: value}` — a declared name rewrites that
        /// field of the row, any other name rides the dynamic segment. Runs the
        /// index entry lifecycle and the reduce calling discipline.
        ///
        /// `replace=True` makes the map the WHOLE document: a declared field it
        /// does not name takes the schema's default, and an unnamed dynamic
        /// field is removed. `replace=False` (the default) touches only the
        /// names it carries. One write verb here too (ADR-0035 as amended).
        #[pyo3(signature = (pkey, document, replace=None))]
        fn put(
            &self,
            pkey: Vec<u8>,
            document: &Bound<'_, PyAny>,
            replace: Option<bool>,
        ) -> PyResult<()> {
            // Schema-coerced for a DECLARED name (Python ints carry no width; the
            // schema kind drives the coercion — same rule as the codec surface),
            // and typed by the Python value for any other name, which rides the
            // dynamic segment (ADR-0035). A path index (ADR-0029) indexes exactly
            // that segment, so refusing undeclared names here would make
            // `add_path_index` a face nothing can feed.
            let map = py_values_to_map_any(&self.schema, document)?;
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if replace.unwrap_or(false) {
                inner.put(&pkey, (&map,))
            } else {
                inner.put(&pkey, &map)
            }
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
    ) -> PyResult<Dict> {
        let dict = values
            .cast::<pyo3::types::PyDict>()
            .map_err(|_| PyTypeError::new_err("values must be a dict {field: value}"))?;
        let mut out = Dict::new();
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
            let mut obj = Dict::new();
            for (k, item) in dict.iter() {
                let key: String = k.extract()?;
                obj.insert(key.clone(), py_dynamic_value(&key, &item)?);
            }
            return Ok(Value::Map(obj));
        }
        Err(PyTypeError::new_err(format!(
            "field `{name}`: an undeclared name takes str / int / float / bool / bytes / list / dict / None"
        )))
    }

    /// Schema-coerced dict → Dict (ints carry no Python width; the
    /// schema kind drives the coercion).
    fn py_values_to_map(
        schema: &okm_core::schema::CollectionSchema,
        values: &Bound<'_, PyAny>,
    ) -> PyResult<Dict> {
        let dict = values
            .cast::<pyo3::types::PyDict>()
            .map_err(|_| PyTypeError::new_err("values must be a dict {field: value}"))?;
        let mut out = Dict::new();
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
            FieldType::Vector(_) => Value::Bytes(
                v.extract::<Vec<u8>>()
                    .map_err(|_| bad("flat LE vector bytes"))?,
            ),
        })
    }

    // =================================================================
    // The dynamic graph (ADR-0017 §4 / ADR-0066) — the SECOND face
    // =================================================================
    //
    // A graph is not a wider Collection: its endpoints are `NodeRef`s, its
    // edge payload is a `Dict`, its clusters are rows of their own four
    // faces, and its two verbs (`incorporate` / `retract`) take a
    // fragment. So it is a second `#[pyclass]` over the same store — the
    // dynamic form is schema-as-data, which is exactly what lets a graph
    // and a document collection coexist in one keyspace.

    /// A node reference across the boundary: `{"ns": 16, "key": b"…"}` in,
    /// the same shape out — so a step's `far` can be fed straight back into
    /// `out` / `inn` / a delete. A `(ns, key)` pair is accepted too: it is the
    /// spelling a caller writing a fixture by hand reaches for.
    fn node_of(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<NodeRef> {
        if let Ok(dict) = obj.cast::<pyo3::types::PyDict>() {
            let ns: u16 = dict
                .get_item("ns")?
                .ok_or_else(|| PyValueError::new_err(format!("{what}: a node dict needs `ns`")))?
                .extract()?;
            let key: Vec<u8> = dict
                .get_item("key")?
                .ok_or_else(|| PyValueError::new_err(format!("{what}: a node dict needs `key`")))?
                .extract()?;
            return Ok(NodeRef::new(&ns.to_be_bytes(), &key));
        }
        obj.extract::<(u16, Vec<u8>)>()
            .map(|(ns, key)| NodeRef::new(&ns.to_be_bytes(), &key))
            .map_err(|_| {
                PyTypeError::new_err(format!(
                    "{what}: a node is {{\"ns\": int, \"key\": bytes}} or (ns, key)"
                ))
            })
    }

    /// `NodeRef` → `{"ns": int, "key": bytes}` — the one shape the node reads
    /// and the traversal steps answer with.
    fn node_to_py(py: Python<'_>, node: &NodeRef) -> PyResult<Py<PyAny>> {
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("ns", node.ns_id())?;
        dict.set_item("key", node.pkey.clone())?;
        Ok(dict.into_any().unbind())
    }

    /// A cluster kind by byte or by the vocabulary's name: the four the ADR
    /// fixes, spelled the way a reader of the docs would spell them.
    fn kind_of(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Kind> {
        if let Ok(name) = obj.extract::<String>() {
            return match name.to_ascii_lowercase().as_str() {
                "community" => Ok(Kind::COMMUNITY),
                "event" => Ok(Kind::EVENT),
                "subgraph" => Ok(Kind::SUBGRAPH),
                "scope" => Ok(Kind::SCOPE),
                other => Err(PyValueError::new_err(format!(
                    "{what}: kind is community / event / subgraph / scope or its byte, got `{other}`"
                ))),
            };
        }
        let byte: u8 = obj.extract().map_err(|_| {
            PyTypeError::new_err(format!(
                "{what}: kind is an int or one of community / event / subgraph / scope"
            ))
        })?;
        Ok(Kind(byte))
    }

    /// One `{name: value}` dict → the dynamic segment's `Dict`. Every name here
    /// is runtime data (an edge's attributes, a cluster payload, a reducer's
    /// fold), so the values type themselves — the same rule the document path
    /// follows for an undeclared name (ADR-0035).
    fn dyn_map_of(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Dict> {
        let dict = obj
            .cast::<pyo3::types::PyDict>()
            .map_err(|_| PyTypeError::new_err(format!("{what} must be a dict {{name: value}}")))?;
        let mut out = Dict::new();
        for (k, v) in dict.iter() {
            let name: String = k.extract()?;
            let value = py_dynamic_value(&name, &v)?;
            out.insert(name, value);
        }
        Ok(out)
    }

    /// The required member of a fragment/declaration dict.
    fn need<'py>(
        dict: &Bound<'py, pyo3::types::PyDict>,
        name: &str,
        what: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        dict.get_item(name)?
            .ok_or_else(|| PyValueError::new_err(format!("{what}: missing `{name}`")))
    }

    /// A sequence entry list (`edges`, `clusters`, `nodes`, `remove`).
    fn entries<'py>(obj: &Bound<'py, PyAny>, what: &str) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let iter = obj
            .try_iter()
            .map_err(|_| PyTypeError::new_err(format!("{what} must be a list")))?;
        let mut out = Vec::new();
        for item in iter {
            out.push(item?);
        }
        Ok(out)
    }

    /// One `edges` entry as a declaration: `{src, dst, predicate, attrs?,
    /// edge_id?}`. `edge_id` names an edge that is already there — the update
    /// path (a re-assertion folds this fragment's reducer over the stored
    /// attributes) and the restore path (an id no row holds creates it).
    fn edge_decl_of(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Decl> {
        let dict = obj.cast::<pyo3::types::PyDict>().map_err(|_| {
            PyTypeError::new_err(format!(
                "{what}: an edge declaration is a dict \
                 {{src, dst, predicate, attrs?, edge_id?}}"
            ))
        })?;
        let src = node_of(&need(dict, "src", what)?, &format!("{what}.src"))?;
        let dst = node_of(&need(dict, "dst", what)?, &format!("{what}.dst"))?;
        let predicate: String = need(dict, "predicate", what)?.extract().map_err(|_| {
            PyTypeError::new_err(format!("{what}: `predicate` is a str (the name, never an id)"))
        })?;
        let attrs = match dict.get_item("attrs")? {
            Some(v) if !v.is_none() => dyn_map_of(&v, &format!("{what}.attrs"))?,
            _ => Dict::new(),
        };
        let edge = DynEdge {
            src,
            dst,
            predicate,
            attrs,
        };
        Ok(match dict.get_item("edge_id")? {
            Some(v) if !v.is_none() => Decl::edge_at(edge, v.extract::<u64>()?),
            _ => Decl::edge(edge),
        })
    }

    /// One cluster entry as a DECLARATION: `{kind, payload?, parent?, id?}`.
    /// `id` incorporates into the cluster already there (`None` allocates one
    /// and reports it), `parent` nests it under another.
    fn cluster_decl_of(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Cluster> {
        let dict = obj.cast::<pyo3::types::PyDict>().map_err(|_| {
            PyTypeError::new_err(format!(
                "{what}: a cluster declaration is a dict {{kind, payload?, parent?, id?}}"
            ))
        })?;
        let kind = kind_of(&need(dict, "kind", what)?, what)?;
        let payload = match dict.get_item("payload")? {
            Some(v) if !v.is_none() => dyn_map_of(&v, &format!("{what}.payload"))?,
            _ => Dict::new(),
        };
        let cluster = match dict.get_item("id")? {
            Some(v) if !v.is_none() => Cluster::into(kind, v.extract::<u64>()?, payload),
            _ => Cluster::new(kind, payload),
        };
        let parent: u64 = match dict.get_item("parent")? {
            Some(v) if !v.is_none() => v.extract()?,
            _ => 0,
        };
        Ok(if parent == 0 {
            cluster
        } else {
            cluster.under(parent)
        })
    }

    /// One `{kind, id}` pair: the identity every cluster verb speaks.
    fn cluster_ref_of(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<ClusterRef> {
        let dict = obj
            .cast::<pyo3::types::PyDict>()
            .map_err(|_| PyTypeError::new_err(format!("{what}: a cluster is a dict {{kind, id}}")))?;
        Ok(ClusterRef::new(
            kind_of(&need(dict, "kind", what)?, what)?,
            need(dict, "id", what)?.extract::<u64>()?,
        ))
    }

    /// The reducer's value: a DICT OF FIELDS to fold, and never a callable.
    ///
    /// The Rust face's conflict hooks are `Fn(&Dict, &Dict) -> Dict`, so an
    /// honest callable would have to hand Python both decoded payloads and read
    /// the answer back as an encoded `Dict` on every entry of the batch — a
    /// re-encode per edge for what is, on this form, plain data. So the dynamic
    /// equivalent is data too: every name in the dict is written into every
    /// payload the batch resolves, and the folds that need to read before writing
    /// stay in Rust (they are the ones the payload's own shape cannot express).
    fn fold_of(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Dict> {
        if obj.is_callable() {
            return Err(PyValueError::new_err(format!(
                "{what} takes a dict of fields to fold, not a callable: a callable would be handed \
                 both payloads and would have to re-encode the whole row per entry, and on this \
                 form the fold is data"
            )));
        }
        dyn_map_of(obj, what)
    }

    /// Which verb a fragment is being read for. The two verbs share ONE
    /// fragment shape — the delete keys (`delete_edges`, `delete_nodes`,
    /// `delete_clusters`, and integer `edges`) belong to `retract`, the write
    /// keys to `incorporate` — so a mixed dict parses on both sides and the
    /// REFUSAL is the Rust verb's own, message included. The only place the
    /// verb changes a reading is a cluster entry that names nothing but
    /// `kind`/`id`: a target for `retract`, a declaration for `incorporate`.
    #[derive(Clone, Copy, PartialEq)]
    enum ReadFor {
        Write,
        Delete,
    }

    /// A fragment dict → `DynFragment`. Nothing here decides whether the mix is
    /// legal: `incorporate` refuses a fragment carrying deletions and `retract`
    /// refuses one carrying writes, with the same strings the Rust face raises.
    fn fragment_of(obj: &Bound<'_, PyAny>, verb: ReadFor) -> PyResult<DynFragment> {
        let dict = obj
            .cast::<pyo3::types::PyDict>()
            .map_err(|_| PyTypeError::new_err("a fragment must be a dict"))?;
        let mut decls: Vec<Decl> = Vec::new();
        let mut edge_deletes: Vec<u64> = Vec::new();
        let mut node_deletes: Vec<NodeRef> = Vec::new();
        let mut cluster_decls: Vec<Cluster> = Vec::new();
        let mut cluster_deletes: Vec<ClusterRef> = Vec::new();
        let mut removes: Vec<ClusterRef> = Vec::new();

        for item in list_of(dict, "edges")? {
            // An INTEGER is an edge id — a delete, whatever verb is reading it.
            // A dict is a declaration. Reading both here is what lets
            // `incorporate` answer with "that is retract's verb" instead of a
            // type complaint about the shape the caller fed the other verb.
            match item.extract::<u64>() {
                Ok(id) => edge_deletes.push(id),
                Err(_) => decls.push(edge_decl_of(&item, "edges")?),
            };
        }

        for item in list_of(dict, "clusters")? {
            if verb == ReadFor::Delete && cluster_is_target(&item) {
                cluster_deletes.push(cluster_ref_of(&item, "clusters")?);
            } else {
                cluster_decls.push(cluster_decl_of(&item, "clusters")?);
            }
        }

        for item in list_of(dict, "delete_edges")? {
            edge_deletes.push(item.extract::<u64>()?);
        }
        for item in list_of(dict, "delete_nodes")? {
            node_deletes.push(node_of(&item, "delete_nodes")?);
        }
        for item in list_of(dict, "delete_clusters")? {
            cluster_deletes.push(cluster_ref_of(&item, "delete_clusters")?);
        }
        for item in list_of(dict, "remove")? {
            removes.push(cluster_ref_of(&item, "remove")?);
        }

        // The declarations are the fragment's seed (`DynFragment::new` is the
        // only door that takes them), and everything else attaches after. The
        // order among the cluster operations is not a contract: the plan is a
        // map keyed by cluster, and a write's `add` wins over a `remove`.
        let mut fragment = DynFragment::new(decls);
        for id in edge_deletes {
            fragment.delete_edge(id);
        }
        for node in node_deletes {
            fragment.delete_node(node);
        }
        for cluster in cluster_decls {
            fragment.cluster(cluster);
        }
        for cluster in cluster_deletes {
            fragment.delete(cluster);
        }
        for cluster in removes {
            fragment.remove(cluster);
        }
        install_conflict_hooks(&mut fragment, dict)?;
        Ok(fragment)
    }

    /// One of a fragment's entry lists, or nothing when the key is absent (or
    /// carries `None`, which reads as absent — a caller that built its dict
    /// conditionally should not have to delete the key).
    fn list_of<'py>(
        dict: &Bound<'py, pyo3::types::PyDict>,
        name: &str,
    ) -> PyResult<Vec<Bound<'py, PyAny>>> {
        match dict.get_item(name)? {
            Some(v) if !v.is_none() => entries(&v, name),
            _ => Ok(Vec::new()),
        }
    }

    /// The ONE place this binding installs a conflict hook on a fragment.
    ///
    /// Everything a Python fragment can say about payload resolution arrives
    /// here — `{"reduce_edge": {…}, "reduce_cluster": {…}}` — and this function
    /// is the only code that names the install calls. That is deliberate: the
    /// Rust builders take `Fn(&old, &new) -> row`, and keeping the install calls
    /// in one function makes a re-signing of them a one-function diff instead of
    /// a hunt through the fragment parser.
    ///
    /// What it installs is the FOLD the binding states: a Python callable would
    /// re-encode the whole payload per entry, so the fold is a dict of fields
    /// instead — "these names are written onto every payload this batch writes"
    /// (see `Graph.incorporate`). The hook is where that meets the two-argument
    /// shape: the STORED payload is the base (so an update keeps what the row
    /// already had), the declaration's own entries go over it, and the fields
    /// last, so a field the caller named always wins.
    fn install_conflict_hooks(
        fragment: &mut DynFragment,
        spec: &Bound<'_, pyo3::types::PyDict>,
    ) -> PyResult<()> {
        if let Some(v) = spec.get_item("reduce_edge")? {
            if !v.is_none() {
                let fields = fold_of(&v, "reduce_edge")?;
                fragment.on_edge_conflict(move |stored: &Dict, declared: &Dict| {
                    let mut row = stored.clone();
                    for (name, value) in declared.iter() {
                        row.insert(name.clone(), value.clone());
                    }
                    for (name, value) in &fields {
                        row.insert(name.clone(), value.clone());
                    }
                    row
                });
            }
        }
        if let Some(v) = spec.get_item("reduce_cluster")? {
            if !v.is_none() {
                let fields = fold_of(&v, "reduce_cluster")?;
                fragment.on_cluster_conflict(move |stored: &Dict, declared: &Dict| {
                    let mut row = stored.clone();
                    for (name, value) in declared.iter() {
                        row.insert(name.clone(), value.clone());
                    }
                    for (name, value) in &fields {
                        row.insert(name.clone(), value.clone());
                    }
                    row
                });
            }
        }
        Ok(())
    }

    /// Whether a cluster entry is a DELETE TARGET (kind + id, nothing else) —
    /// the one reading that depends on the verb.
    fn cluster_is_target(obj: &Bound<'_, PyAny>) -> bool {
        let Ok(dict) = obj.cast::<pyo3::types::PyDict>() else {
            return false;
        };
        let names = ["payload", "parent"];
        !names.iter().any(|n| {
            dict.get_item(*n)
                .ok()
                .flatten()
                .is_some_and(|v| !v.is_none())
        })
    }

    /// The step dict: the edge's id, the FAR endpoint, the head it came from and
    /// the entry's own key — the last one because it is the position
    /// `okm-query`'s operators speak, so a walk can feed a [`KeySet`] directly.
    fn step_to_py(py: Python<'_>, key: &[u8], step: &EdgeStep) -> PyResult<Py<PyAny>> {
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("edge_id", step.edge_id)?;
        dict.set_item("far", node_to_py(py, &step.far)?)?;
        dict.set_item("key", key.to_vec())?;
        let head = pyo3::types::PyDict::new(py);
        head.set_item("src", node_to_py(py, &step.head.src)?)?;
        head.set_item("dst", node_to_py(py, &step.head.dst)?)?;
        head.set_item("predicate", step.head.predicate)?;
        dict.set_item("head", head)?;
        Ok(dict.into_any().unbind())
    }

    fn write_report_to_py(py: Python<'_>, report: &WriteReport) -> PyResult<Py<PyAny>> {
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("edge_ids", report.edge_ids.clone())?;
        dict.set_item("hyper_ids", report.hyper_ids.clone())?;
        Ok(dict.into_any().unbind())
    }

    fn retract_report_to_py(py: Python<'_>, report: &RetractReport) -> PyResult<Py<PyAny>> {
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("nodes", report.nodes)?;
        dict.set_item("edges", report.edges)?;
        dict.set_item("memberships", report.memberships)?;
        Ok(dict.into_any().unbind())
    }

    fn cluster_ref_to_py(py: Python<'_>, cluster: &ClusterRef) -> PyResult<Py<PyAny>> {
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("kind", cluster.kind.byte())?;
        dict.set_item("id", cluster.id)?;
        Ok(dict.into_any().unbind())
    }

    /// A dynamic GRAPH over an embedded (or injected) store: the write verbs,
    /// the traversal streams and the cluster reads of ADR-0017 §4.
    ///
    /// It shares the store, not the declaration: a `Collection` beside it on the
    /// same engine is a different table in the same keyspace, because the
    /// dynamic form is schema-as-data (`Graph.from_collection` is the one-liner
    /// for "the graph over this collection's rows").
    #[pyclass]
    pub struct Graph {
        /// The Rust face. `Mutex` because `incorporate`/`retract` take `&mut`
        /// (they warm the vocabulary caches) while the reads take `&self`, and
        /// Python may reach both from two threads.
        inner: Arc<Mutex<DynGraph<EngineBox>>>,
        /// The same engine as a HANDLE (a clone is the keyspace, not a copy) —
        /// what a traversal cursor owns, because it cannot hold a borrow into
        /// `inner` across its own calls.
        store: EngineBox,
        ns: u16,
    }

    impl Graph {
        /// Assemble over an engine the caller owns. A schema whose `ns_prefix`
        /// names another ns is REFUSED here rather than asserted in
        /// `okm_dynamic::Graph::new`: the ns and the schema travel as a pair
        /// (ADR-0025), and a binding's job is to report the disagreement, not
        /// to abort the interpreter on it.
        pub fn assemble(
            store: EngineBox,
            schema: okm_core::schema::CollectionSchema,
            ns: u16,
        ) -> Result<Self, String> {
            if !schema.ns_prefix.is_empty() && schema.ns_prefix != ns.to_be_bytes() {
                return Err(format!(
                    "graph assembly: ns {ns} disagrees with the schema's declared ns {:?} — the \
                     pair comes from one declaration, never from two",
                    schema.ns_prefix
                ));
            }
            let inner = DynGraph::new(store.clone(), ns, schema.clone());
            Ok(Graph {
                inner: Arc::new(Mutex::new(inner)),
                store,
                ns,
            })
        }

        /// The host-injected face (plain Rust, like `Collection::with_store`):
        /// build a graph over an ENGINE THE HOST OWNS. Not a `#[pymethods]`
        /// shape — `Arc<dyn Engine>` is not marshalable — which is why it lives
        /// beside the shells rather than in them.
        pub fn with_store(
            engine: Arc<dyn Engine>,
            schema: &okm_core::schema::CollectionSchema,
            ns: u16,
        ) -> Result<Self, String> {
            Self::assemble(EngineBox::Injected(engine), schema.clone(), ns)
        }

        /// The engine handle this graph reads through.
        pub fn store_handle(&self) -> EngineBox {
            self.store.clone()
        }

        pub fn ns_id(&self) -> u16 {
            self.ns
        }

        /// The write verb, plain Rust: `incorporate` with a fragment already
        /// built (the shells below add only the dict → fragment coercion).
        pub fn incorporate_dyn(&self, fragment: DynFragment) -> Result<WriteReport, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .incorporate(fragment)
        }

        /// The delete verb, plain Rust.
        pub fn retract_dyn(&self, fragment: DynFragment) -> Result<RetractReport, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retract(fragment)
        }

        /// One hop out of `node`, as an OWNED cursor.
        pub fn out_dyn(&self, node: &NodeRef) -> EdgeStream {
            EdgeStream::open(self.store.clone(), self.ns, Direction::Out, node.clone())
        }

        /// One hop in to `node` — the mirror face.
        pub fn inn_dyn(&self, node: &NodeRef) -> EdgeStream {
            EdgeStream::open(self.store.clone(), self.ns, Direction::In, node.clone())
        }

        pub fn member_of_dyn(&self, kind: Kind, cluster: u64, edge_id: u64) -> Result<bool, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .member_of(kind, cluster, edge_id)
        }

        pub fn members_dyn(&self, kind: Kind, cluster: u64) -> Result<Vec<u64>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .members(kind, cluster)
        }

        pub fn children_dyn(&self, kind: Kind, parent: u64) -> Result<Vec<u64>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .children(kind, parent)
        }

        pub fn parent_dyn(&self, kind: Kind, id: u64) -> Result<u64, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .parent(kind, id)
        }

        pub fn clusters_dyn(&self, kind: Kind) -> Result<Vec<u64>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clusters(kind)
        }

        pub fn all_clusters_dyn(&self) -> Result<Vec<ClusterRef>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .all_clusters()
        }

        pub fn member_count_dyn(&self, kind: Kind, id: u64) -> Result<u64, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .member_count(kind, id)
        }

        pub fn clusters_of_dyn(&self, edge_id: u64) -> Result<Vec<ClusterRef>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clusters_of(edge_id)
        }

        /// One edge, decoded: endpoints, predicate by NAME, attributes.
        pub fn edge_dyn(&self, edge_id: u64) -> Result<Option<DynEdge>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_edge(edge_id)
        }

        pub fn lookup_dyn(
            &self,
            kind: Kind,
            id: u64,
        ) -> Result<Option<okm_dynamic::ClusterRow>, String> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .lookup(kind, id)
        }
    }

    #[pymethods]
    impl Graph {
        /// A graph over a FRESH in-process store (embedded mode: Python is the
        /// only writer). For a graph over an existing collection's rows see
        /// `from_collection`.
        #[new]
        fn new(schema: &Schema, ns: u16) -> PyResult<Self> {
            Self::assemble(
                EngineBox::Test(okm_async::TestStore::slatedb_mem()),
                schema.inner.as_ref().clone(),
                ns,
            )
            .map_err(PyValueError::new_err)
        }

        /// The graph over the SAME store a `Collection` writes — the
        /// schema-as-data case: one keyspace, a document table and a graph
        /// beside it. `schema` defaults to the collection's own; pass one that
        /// declares a `members` set (the graph declaration's node collections)
        /// to have endpoints checked against it.
        #[staticmethod]
        #[pyo3(signature = (collection, schema=None))]
        fn from_collection(collection: &Collection, schema: Option<&Schema>) -> PyResult<Self> {
            let schema = match schema {
                Some(s) => s.inner.as_ref().clone(),
                None => collection.schema.clone(),
            };
            Self::assemble(collection.store_handle(), schema, collection.ns)
                .map_err(PyValueError::new_err)
        }

        /// Write one fragment — the verb (ADR-0066 §7, dynamic form). One engine
        /// commit, so a reader sees all of it or none of it, and a fragment that
        /// is refused writes nothing.
        ///
        /// The fragment is a dict:
        ///
        /// ```text
        /// {
        ///   "edges":     [{src, dst, predicate, attrs?, edge_id?}, …],
        ///   "clusters":  [{kind, payload?, parent?, id?}, …],
        ///   "reduce_edge":    {name: value},   # folded onto every edge payload
        ///   "reduce_cluster": {name: value},   # folded onto every declared payload
        ///   "remove":    [{kind, id}, …],      # detach this batch's edges
        /// }
        /// ```
        ///
        /// `kind` is `community` / `event` / `subgraph` / `scope` or its byte.
        /// A node is `{"ns": int, "key": bytes}` or `(ns, key)`.
        ///
        /// The reducers take a DICT OF FIELDS, not a callable: the Rust hooks are
        /// `Fn(&Dict, &Dict) -> Dict`, so a Python callable would be handed both
        /// payloads and would re-encode the whole row for every entry — and on this
        /// form the payload is data, so the fold is data too. The folds a dict
        /// cannot express (accumulate, read-then-write) stay in Rust.
        ///
        /// A fragment that carries deletions is REFUSED by this verb with
        /// okm-dynamic's own message ("incorporate writes; … that is retract's
        /// verb"), as `ValueError` — the same refusal the Rust face raises.
        ///
        /// Answers `{"edge_ids": [...], "hyper_ids": [...]}`: the ids this write
        /// allocated (an `edge_id`/cluster `id` the caller supplied is not one of
        /// them).
        fn incorporate(&self, fragment: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
            let fragment = fragment_of(fragment, ReadFor::Write)?;
            let report = self
                .incorporate_dyn(fragment)
                .map_err(PyValueError::new_err)?;
            Python::attach(|py| write_report_to_py(py, &report))
        }

        /// Remove — the inverse verb, same fragment shape:
        ///
        /// ```text
        /// {
        ///   "edges":            [edge_id, …],        # or declarations, to be refused
        ///   "nodes":            [{ns, key} | (ns, key), …],
        ///   "clusters":         [{kind, id}, …],
        ///   "delete_edges":     [edge_id, …],
        ///   "delete_nodes":     [{ns, key}, …],
        ///   "delete_clusters":  [{kind, id}, …],
        /// }
        /// ```
        ///
        /// Deleting a NODE takes every edge incident to it (both directions, the
        /// predicate faces, the payload row, the dynamic segment, those edges'
        /// membership rows). Deleting an EDGE leaves both endpoints standing,
        /// even when they are then connected to nothing. An id nobody holds is
        /// nothing to do, not a refusal.
        ///
        /// A fragment that declares writes (edge/cluster declarations, a
        /// reducer, a `remove`) is refused with okm-dynamic's message
        /// ("retract removes; … that is incorporate's verb").
        ///
        /// Answers `{"nodes": n, "edges": n, "memberships": n}`.
        fn retract(&self, fragment: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
            let fragment = fragment_of(fragment, ReadFor::Delete)?;
            let report = self.retract_dyn(fragment).map_err(PyValueError::new_err)?;
            Python::attach(|py| retract_report_to_py(py, &report))
        }

        /// One edge by id: `{edge_id, src, dst, predicate, attrs}`, or `None`.
        /// The predicate comes back by NAME (the vocabulary resolves it), and the
        /// attributes as the map the edge was written with.
        fn edge(&self, edge_id: u64) -> PyResult<Option<Py<PyAny>>> {
            let Some(edge) = self.edge_dyn(edge_id).map_err(PyValueError::new_err)? else {
                return Ok(None);
            };
            Python::attach(|py| {
                let dict = pyo3::types::PyDict::new(py);
                dict.set_item("edge_id", edge_id)?;
                dict.set_item("src", node_to_py(py, &edge.src)?)?;
                dict.set_item("dst", node_to_py(py, &edge.dst)?)?;
                dict.set_item("predicate", edge.predicate.clone())?;
                dict.set_item("attrs", map_to_dict(py, &edge.attrs)?)?;
                Ok(Some(dict.into_any().unbind()))
            })
        }

        // ---- traversal: one hop, as a real iterator ----

        /// The out-edges of `node`, as a LAZY iterator: one entry per `next()`,
        /// read straight off the face, no row fetched (the entry key names both
        /// endpoints).
        ///
        /// Dropping the stream stops the scan where it stands — that is what the
        /// laziness is for, and `pulled` reports how many entries the walk
        /// actually read, so the saving is visible rather than claimed.
        fn out(&self, node: &Bound<'_, PyAny>) -> PyResult<EdgeStream> {
            Ok(self.out_dyn(&node_of(node, "out")?))
        }

        /// The in-edges of `node`, the mirror face. Its step's `far` is the
        /// SOURCE, and `head.dst` is the node the walk started from.
        fn inn(&self, node: &Bound<'_, PyAny>) -> PyResult<EdgeStream> {
            Ok(self.inn_dyn(&node_of(node, "inn")?))
        }

        /// One hop out — the chainable spelling of `out`: two hops is
        /// `flat_map` over the first walk's `far`, and the depth lives in the
        /// caller's iterator chain.
        fn walk(&self, node: &Bound<'_, PyAny>) -> PyResult<EdgeStream> {
            self.out(node)
        }

        // ---- cluster reads ----

        /// Is `edge_id` a member of cluster `(kind, cluster)`? One point read.
        fn member_of(&self, kind: &Bound<'_, PyAny>, cluster: u64, edge_id: u64) -> PyResult<bool> {
            let kind = kind_of(kind, "member_of")?;
            self.member_of_dyn(kind, cluster, edge_id)
                .map_err(PyValueError::new_err)
        }

        /// The member edges of one cluster, in edge-id order: one prefix scan of
        /// the authority face.
        fn members(&self, kind: &Bound<'_, PyAny>, cluster: u64) -> PyResult<Vec<u64>> {
            let kind = kind_of(kind, "members")?;
            self.members_dyn(kind, cluster).map_err(PyValueError::new_err)
        }

        /// The children of one cluster: one scan of the child index.
        fn children(&self, kind: &Bound<'_, PyAny>, parent: u64) -> PyResult<Vec<u64>> {
            let kind = kind_of(kind, "children")?;
            self.children_dyn(kind, parent)
                .map_err(PyValueError::new_err)
        }

        /// One cluster row: `{kind, id, parent, member_count, payload}`, or
        /// `None` when no row holds it. `parent` 0 means "no parent";
        /// `member_count` is the number the row states.
        fn lookup(&self, kind: &Bound<'_, PyAny>, id: u64) -> PyResult<Option<Py<PyAny>>> {
            let kind = kind_of(kind, "lookup")?;
            let Some(row) = self.lookup_dyn(kind, id).map_err(PyValueError::new_err)? else {
                return Ok(None);
            };
            Python::attach(|py| {
                let dict = pyo3::types::PyDict::new(py);
                dict.set_item("kind", kind.byte())?;
                dict.set_item("id", id)?;
                dict.set_item("parent", row.parent)?;
                dict.set_item("member_count", row.member_count())?;
                dict.set_item("payload", map_to_dict(py, &row.payload)?)?;
                Ok(Some(dict.into_any().unbind()))
            })
        }

        /// The parent of one cluster — one point read; `0` = no parent.
        fn parent(&self, kind: &Bound<'_, PyAny>, id: u64) -> PyResult<u64> {
            let kind = kind_of(kind, "parent")?;
            self.parent_dyn(kind, id).map_err(PyValueError::new_err)
        }

        /// Every cluster of one kind, in id order.
        fn clusters(&self, kind: &Bound<'_, PyAny>) -> PyResult<Vec<u64>> {
            let kind = kind_of(kind, "clusters")?;
            self.clusters_dyn(kind).map_err(PyValueError::new_err)
        }

        /// Every cluster of every kind: `[{"kind": int, "id": int}]`.
        fn all_clusters(&self) -> PyResult<Vec<Py<PyAny>>> {
            let clusters = self.all_clusters_dyn().map_err(PyValueError::new_err)?;
            Python::attach(|py| {
                clusters
                    .iter()
                    .map(|c| cluster_ref_to_py(py, c))
                    .collect()
            })
        }

        /// The member count the cluster row states (0 when it has no row).
        fn member_count(&self, kind: &Bound<'_, PyAny>, id: u64) -> PyResult<u64> {
            let kind = kind_of(kind, "member_count")?;
            self.member_count_dyn(kind, id)
                .map_err(PyValueError::new_err)
        }

        /// Every cluster one edge belongs to: `[{"kind": int, "id": int}]`.
        ///
        /// The reverse of `members`, one scan of the derived mirror. There is no
        /// node form: on this form a node is an endpoint and nothing else, and
        /// membership is a property of EDGES — a node is answered by asking
        /// about its edges (`clusters_of(step["edge_id"])` inside a walk).
        fn clusters_of(&self, edge_id: u64) -> PyResult<Vec<Py<PyAny>>> {
            let clusters = self
                .clusters_of_dyn(edge_id)
                .map_err(PyValueError::new_err)?;
            Python::attach(|py| {
                clusters
                    .iter()
                    .map(|c| cluster_ref_to_py(py, c))
                    .collect()
            })
        }
    }

    /// One node's edges, one direction, as a LAZY, ORDERED cursor — the binding
    /// side of `okm_dynamic::DynEdgeStream`.
    ///
    /// It is a cursor of its OWN because a `#[pyclass]` cannot hold the borrowed
    /// stream: a `DynEdgeStream<'a, S>` borrows the store, and the store lives
    /// inside the graph, so the borrow could not outlive one call. This type
    /// OWNS the engine handle (a clone is the keyspace) and the scan it opened,
    /// and asks okm-dynamic what each entry means (`edge_step`) and where a walk
    /// starts (`edge_face_prefix`) — the layout stays in one place; only the
    /// position lives here.
    ///
    /// The contracts it inherits, unchanged:
    ///
    /// - **One entry per step.** Construction puts the first entry on top and
    ///   each `next` hands it back and pulls at most one more, so a consumer
    ///   reads at most one entry ahead and dropping the cursor stops the scan.
    /// - **Ordered by entry key** — the far endpoint's byte order, then the id.
    /// - **Cycle-unaware.** One node's edges and stop; a visited set would price
    ///   every walk for a question only the caller can ask.
    ///
    /// `unsendable` because the engine's scan cursor is not `Sync` (a native
    /// backend's snapshot and range are thread-affine), and a cursor is a
    /// POSITION anyway: it belongs to the thread that opened it. The `Graph` it
    /// came from stays sendable — opening a walk per thread is the shape.
    #[pyclass(unsendable)]
    pub struct EdgeStream {
        store: EngineBox,
        ns: u16,
        dir: Direction,
        node: NodeRef,
        /// The pushed-down predicate id, or `None` for the untyped face.
        filter: Option<u16>,
        /// The face cursor; `None` means spent before its first item (a
        /// predicate nobody interned has no face to walk).
        scan: Option<okm_core::storage::ScanIter>,
        /// Bytes of key before the far endpoint (the scanned prefix's length).
        suffix_at: usize,
        /// The entry on top; `None` once spent.
        cur: Option<(Vec<u8>, EdgeStep)>,
        /// The fault that ended the walk, if one did.
        err: Option<String>,
        /// Whether `__next__` already raised that fault (it raises once and then
        /// answers `None`, so a Python `for` loop is not interrupted twice).
        err_raised: bool,
        /// Entries the cursor READ — the number the laziness claims are about.
        pulled: usize,
        /// Entries it ANSWERED.
        steps: usize,
    }

    impl EdgeStream {
        /// A cursor over one node's edges. Plain Rust, so a host test can drive
        /// a traversal without an interpreter (the same reason `Collection`
        /// keeps its `_doc` methods beside the `#[pymethods]` shell).
        pub fn open(store: EngineBox, ns: u16, dir: Direction, node: NodeRef) -> Self {
            let mut stream = EdgeStream {
                store,
                ns,
                dir,
                node,
                filter: None,
                scan: None,
                suffix_at: 0,
                cur: None,
                err: None,
                err_raised: false,
                pulled: 0,
                steps: 0,
            };
            stream.reopen(None);
            stream
        }

        fn prefix(&self) -> Vec<u8> {
            okm_dynamic::edge_face_prefix(
                self.ns.to_be_bytes(),
                self.dir,
                self.filter,
                &self.node,
            )
        }

        /// Point the cursor at (possibly) another face and re-fill it. A filter
        /// change is a different FACE, not a narrower range on the same one, so
        /// the position does not survive — the old scan is dropped and the new
        /// one starts at its own first entry.
        fn reopen(&mut self, filter: Option<u16>) {
            self.filter = filter;
            let prefix = self.prefix();
            self.suffix_at = prefix.len();
            self.cur = None;
            self.err = None;
            let end = okm_core::storage::prefix_end(&prefix);
            self.scan = Some(self.store.scan_range(&prefix, end.as_deref()));
            self.fill();
        }

        /// Put the next decodable entry on top, or clear it when the face ends.
        /// An entry whose key this face cannot carry is SKIPPED rather than
        /// guessed at — it belongs to some other layout.
        fn fill(&mut self) {
            self.cur = None;
            loop {
                let next = match self.scan.as_mut() {
                    Some(scan) => scan.next(),
                    None => return,
                };
                match next {
                    None => return,
                    Some(Err(e)) => {
                        self.err = Some(e.to_string());
                        self.scan = None;
                        return;
                    }
                    Some(Ok((key, _))) => {
                        self.pulled += 1;
                        if let Some(step) = okm_dynamic::edge_step(
                            &key,
                            self.suffix_at,
                            self.dir,
                            &self.node,
                            self.filter,
                        ) {
                            self.cur = Some((key, step));
                            return;
                        }
                    }
                }
            }
        }

        /// The next hop — its raw face key and the step it decodes to.
        pub fn next_step(&mut self) -> Option<(Vec<u8>, EdgeStep)> {
            let entry = self.cur.take()?;
            self.fill();
            self.steps += 1;
            Some(entry)
        }

        /// The same hop narrowed to one predicate, PUSHED DOWN onto the
        /// `0x08`/`0x09` prefix: the cursor OPENS on that predicate's entries,
        /// so not one entry of the node's whole face is read and thrown away.
        ///
        /// An unknown name is an EMPTY cursor with no scan behind it at all — no
        /// edge can carry a predicate nobody wrote.
        pub fn filtered(
            store: EngineBox,
            ns: u16,
            dir: Direction,
            node: NodeRef,
            predicate: &str,
        ) -> Result<Self, String> {
            let filter = okm_dynamic::predicate_id_in(&store, &ns.to_be_bytes(), predicate)?;
            let mut stream = EdgeStream {
                store,
                ns,
                dir,
                node,
                filter,
                scan: None,
                suffix_at: 0,
                cur: None,
                err: None,
                err_raised: false,
                pulled: 0,
                steps: 0,
            };
            if let Some(id) = filter {
                stream.reopen(Some(id));
            }
            Ok(stream)
        }

        /// Entries the cursor has READ (the number a laziness claim is about).
        pub fn pulled_count(&self) -> usize {
            self.pulled
        }

        /// Entries it has ANSWERED.
        pub fn step_count(&self) -> usize {
            self.steps
        }

        pub fn error_text(&self) -> Option<&str> {
            self.err.as_deref()
        }

        /// Which face family this cursor walks.
        pub fn direction_of(&self) -> Direction {
            self.dir
        }

        /// The pushed-down predicate id, or `None` for the untyped face.
        pub fn filter_id(&self) -> Option<u16> {
            self.filter
        }
    }

    #[pymethods]
    impl EdgeStream {
        fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
            slf
        }

        /// The next hop: `{edge_id, far: {ns, key}, key, head: {src, dst,
        /// predicate}}`. A spent face answers `StopIteration` — the iterator
        /// protocol, so `for step in g.out(n)` (and `next(it, None)`) work.
        ///
        /// A fault mid-walk RAISES `ValueError` rather than reading as "no more
        /// neighbours" (the distinction an iterator cannot draw, and the one a
        /// caller needs before it trusts an empty answer). It raises once;
        /// `error` keeps the text afterwards.
        fn __next__(&mut self) -> PyResult<Option<Py<PyAny>>> {
            if let Some((key, step)) = self.next_step() {
                return Python::attach(|py| step_to_py(py, &key, &step).map(Some));
            }
            match (&self.err, self.err_raised) {
                (Some(e), false) => {
                    let message = format!("graph traversal: {e}");
                    self.err_raised = true;
                    Err(PyValueError::new_err(message))
                }
                _ => Ok(None),
            }
        }

        /// The same hop, narrowed to one predicate — a NEW iterator over that
        /// predicate's own face (the position restarts there: a filter change is
        /// a different key space, so the old one's position is not a bound in
        /// it). An unknown predicate answers an empty iterator, not an error.
        fn by(&self, predicate: &str) -> PyResult<Self> {
            EdgeStream::filtered(
                self.store.clone(),
                self.ns,
                self.dir,
                self.node.clone(),
                predicate,
            )
            .map_err(PyValueError::new_err)
        }

        /// Entries the cursor has READ. Compare it with the face's size after a
        /// `break`: that difference is the scan the laziness saved.
        #[getter]
        fn pulled(&self) -> usize {
            self.pulled
        }

        /// Entries the cursor has ANSWERED.
        #[getter]
        fn steps(&self) -> usize {
            self.steps
        }

        /// The fault that ended this walk, if one did (`None` for a face that
        /// simply ran out).
        #[getter]
        fn error(&self) -> Option<String> {
            self.err.clone()
        }

        /// `"out"` or `"in"`.
        #[getter]
        fn direction(&self) -> &'static str {
            match self.dir {
                Direction::Out => "out",
                Direction::In => "in",
            }
        }

        /// The pushed-down predicate id, when `by` narrowed this walk.
        #[getter]
        fn predicate(&self) -> Option<u16> {
            self.filter
        }

        /// The node the walk started from.
        #[getter]
        fn node(&self) -> PyResult<Py<PyAny>> {
            Python::attach(|py| node_to_py(py, &self.node))
        }
    }

    /// An ordered, deduplicated set of ENTRY KEYS: the second input of a set
    /// operator, and the shape okm-query's alignment speaks.
    ///
    /// The point is the TWO-STEP semantics, kept instead of degenerating into a
    /// Python loop over ids: `okm_query`'s operators (`intersect`, `union`,
    /// `difference`) run inside one call, and `fetch` turns the surviving keys
    /// into rows in one more. A `for` loop in Python would be a round trip per
    /// id, which is exactly what the operators exist to avoid.
    ///
    /// The algebra is okm-query's; the container is here because neither
    /// okm-dynamic nor okm-core exposes a materialized key set — okm-dynamic
    /// offers ordered STREAMS (`EdgeStream` is a `Seekable` position), and a set
    /// you can hold across calls is a consumer-side container. So this wraps a
    /// sorted `Vec<Vec<u8>>` and hands it to the same operators a scan would
    /// feed; no key ALGEBRA is re-implemented, only the sort/dedup that makes a
    /// Vec a set.
    #[pyclass]
    pub struct KeySet {
        keys: Vec<Vec<u8>>,
    }

    /// One row behind a key set's `fetch`: the key, and the value the store
    /// holds at it (`None` when the key is gone).
    pub type KeyRow = (Vec<u8>, Option<Vec<u8>>);

    impl KeySet {
        /// A set from any key sequence: sorted and deduplicated, which is what
        /// makes it ordered input for the operators.
        pub fn of(mut keys: Vec<Vec<u8>>) -> Self {
            keys.sort_unstable();
            keys.dedup();
            KeySet { keys }
        }

        pub fn keys_ref(&self) -> &[Vec<u8>] {
            &self.keys
        }

        pub fn intersect_dyn(&self, other: &KeySet) -> KeySet {
            use okm_query::OrderedSets;
            KeySet::of(
                self.keys
                    .clone()
                    .into_iter()
                    .intersect(other.keys.clone())
                    .collect(),
            )
        }

        pub fn union_dyn(&self, other: &KeySet) -> KeySet {
            use okm_query::OrderedSets;
            KeySet::of(
                self.keys
                    .clone()
                    .into_iter()
                    .union(other.keys.clone())
                    .collect(),
            )
        }

        pub fn difference_dyn(&self, other: &KeySet) -> KeySet {
            use okm_query::OrderedSets;
            KeySet::of(
                self.keys
                    .clone()
                    .into_iter()
                    .difference(other.keys.clone())
                    .collect(),
            )
        }

        /// The rows behind the surviving keys, in key order: one point read per
        /// key, all of them inside this call. `None` value = the key is not in
        /// the store any more (a normal race on a live store, not a failure).
        pub fn fetch_dyn(&self, store: &EngineBox) -> Result<Vec<KeyRow>, String> {
            let mut rows = Vec::with_capacity(self.keys.len());
            for key in &self.keys {
                let value = store.get(key).map_err(|e| e.to_string())?;
                rows.push((key.clone(), value));
            }
            Ok(rows)
        }
    }

    /// The engine behind a key set's `fetch`: a `Graph` or a `Collection` owns
    /// one, and either is the right store to read the keys from.
    fn store_of(source: &Bound<'_, PyAny>) -> PyResult<EngineBox> {
        if let Ok(graph) = source.cast::<Graph>() {
            return Ok(graph.borrow().store_handle());
        }
        if let Ok(collection) = source.cast::<Collection>() {
            return Ok(collection.borrow().store_handle());
        }
        Err(PyTypeError::new_err(
            "fetch takes the store's owner: a Graph or a Collection",
        ))
    }

    #[pymethods]
    impl KeySet {
        /// A set from a list of entry keys (order and duplicates do not
        /// matter — it is a set).
        #[new]
        #[pyo3(signature = (keys=None))]
        fn new(keys: Option<Vec<Vec<u8>>>) -> Self {
            KeySet::of(keys.unwrap_or_default())
        }

        /// The keys, in ascending byte order.
        fn keys(&self) -> Vec<Vec<u8>> {
            self.keys.clone()
        }

        fn __len__(&self) -> usize {
            self.keys.len()
        }

        fn __contains__(&self, key: Vec<u8>) -> bool {
            self.keys.binary_search(&key).is_ok()
        }

        /// `a & b` — the keys on both sides, once each (okm-query's `intersect`).
        fn __and__(&self, other: &KeySet) -> KeySet {
            self.intersect_dyn(other)
        }

        /// `a | b` — the keys on either side, once each, in key order.
        fn __or__(&self, other: &KeySet) -> KeySet {
            self.union_dyn(other)
        }

        /// `a - b` — the keys on the left with no match on the right.
        fn __sub__(&self, other: &KeySet) -> KeySet {
            self.difference_dyn(other)
        }

        fn __repr__(&self) -> String {
            format!("KeySet({} key(s))", self.keys.len())
        }

        /// The rows behind this set, in key order: `[{"key": bytes, "value":
        /// bytes | None}]`. One call, one read per key — the second step of
        /// "set algebra first, fetch after".
        fn fetch(&self, source: &Bound<'_, PyAny>) -> PyResult<Vec<Py<PyAny>>> {
            let store = store_of(source)?;
            let rows = self.fetch_dyn(&store).map_err(PyValueError::new_err)?;
            Python::attach(|py| {
                rows.iter()
                    .map(|(key, value)| {
                        let dict = pyo3::types::PyDict::new(py);
                        dict.set_item("key", key.clone())?;
                        dict.set_item("value", value.clone())?;
                        Ok(dict.into_any().unbind())
                    })
                    .collect()
            })
        }
    }

    fn codec_err(e: okm_dynamic::CodecError) -> PyErr {
        PyValueError::new_err(format!("{e}"))
    }

    /// okm — OKM dynamic codec + embedded-mode table for Python.
    #[pymodule]
    fn okm(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_class::<Schema>()?;
        m.add_class::<Collection>()?;
        m.add_class::<Graph>()?;
        m.add_class::<EdgeStream>()?;
        m.add_class::<KeySet>()?;
        Ok(())
    }
}

/// The python schema-declaration DSL as an embeddable string (ADR-0026
/// §4): hosts (probe's python carrier, aura) inject this module into
/// actor scripts so `@KeyEncode` / `@DocumentEncode` / `@ok_*` resolve
/// and `assemble_module` produces the storage block — one source, every
/// consumer embedding the same module.
pub const OKM_SCHEMA_PY: &str = include_str!("../okm/okm_schema.py");
