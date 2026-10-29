//! PyO3 bridge for `okm-series` (ADR-0046).
//!
//! What a Python caller gets is the crate's own shape, not a wrapper with opinions:
//! a `Log` whose `append` returns an offset, `read` that starts at an offset,
//! `read_by_time` for a window, `seal`, `retain_before`, and `parts` for the physical
//! summary. Bodies are plain dicts keyed by tag (or by name, if the log was opened with
//! a `tags` map), and values are `str` / `int` / `float` / `bytes`.
//!
//! Errors cross as Python exceptions: an IO failure is `OSError`, everything else a
//! refusal (time went backwards, a missing partition field, a layout mismatch) is
//! `ValueError` — the caller can act on all of them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use ::okm_series::{Body, Codec, Config, DictConfig, Log, Precision, Value};
use pyo3::exceptions::{PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

fn map_error(error: ::okm_series::Error) -> PyErr {
    match error {
        ::okm_series::Error::Io(io) => PyOSError::new_err(io.to_string()),
        other => PyValueError::new_err(other.to_string()),
    }
}

fn precision_of(label: &str) -> PyResult<Precision> {
    Ok(match label {
        "s" => Precision::S,
        "ms" => Precision::Ms,
        "us" => Precision::Us,
        "ns" => Precision::Ns,
        other => {
            return Err(PyValueError::new_err(format!(
                "precision must be s, ms, us or ns, not {other}"
            )))
        }
    })
}

fn precision_label(precision: Precision) -> &'static str {
    precision.label()
}

/// One record read back: its timestamp in the log's precision, its offset in its stream,
/// and the body decoded into a dict.
#[pyclass(module = "okm_series")]
pub struct Record {
    #[pyo3(get)]
    pub ts: u64,
    #[pyo3(get)]
    pub offset: u64,
    body: Py<PyDict>,
}

#[pymethods]
impl Record {
    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> Bound<'py, PyDict> {
        self.body.clone_ref(py).into_bound(py)
    }

    fn __repr__(&self) -> String {
        format!("Record(ts={}, offset={})", self.ts, self.offset)
    }
}

/// A log on disk. Every method takes the GIL briefly and does its IO without it.
#[pyclass(name = "Log", module = "okm_series")]
pub struct PyLog {
    inner: Mutex<Log>,
    tags: HashMap<String, u16>,
    names: HashMap<u16, String>,
    /// The name the record's write time is exposed under when a body is read back.
    /// `None` disables it; `Record.ts` is always there regardless.
    ts_field: Option<String>,
}

#[pymethods]
impl PyLog {
    #[new]
    #[pyo3(signature = (root, *, precision = "ns", bucket_seconds = 3600,
        block_bytes = 64 * 1024, part_bytes = 64 * 1024 * 1024, dictionary = true,
        compress = true, zstd_level = 1, partition_tag = None, tags = None,
        ts_field = Some("_ts".to_string())))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        root: PathBuf,
        precision: &str,
        bucket_seconds: u64,
        block_bytes: usize,
        part_bytes: u64,
        dictionary: bool,
        compress: bool,
        zstd_level: i32,
        partition_tag: Option<u16>,
        tags: Option<HashMap<String, u16>>,
        ts_field: Option<String>,
    ) -> PyResult<Self> {
        let config = Config {
            precision: precision_of(precision)?,
            bucket_seconds,
            block_bytes,
            part_bytes,
            zstd_level,
            block_codec: if compress { Codec::Zstd } else { Codec::Raw },
            dict: if dictionary {
                Some(DictConfig::default())
            } else {
                None
            },
            partition_tag,
            // The time is the log's: a Python caller cannot supply it, and no clock
            // hook is exposed on purpose.
            clock: None,
        };
        let log = Log::open(&root, config).map_err(map_error)?;
        let tags = tags.unwrap_or_default();
        let mut names = HashMap::new();
        for (name, tag) in &tags {
            names.insert(*tag, name.clone());
        }
        Ok(PyLog {
            inner: Mutex::new(log),
            tags,
            names,
            ts_field,
        })
    }

    /// Append one record at the write time; returns its offset (the address `read`
    /// starts from).
    ///
    /// There is no timestamp argument: the log stamps the record when it is written
    /// (ADR-0046 §2). If the application has its own time, put it in the body under a
    /// tag of your choosing — it is data, not the record's address.
    ///
    /// The record is buffered until a block fills, so a reader sees it after `flush`
    /// — use `append_durable` for "on disk when this returns".
    fn append(&self, py: Python<'_>, body: &Bound<'_, PyDict>) -> PyResult<u64> {
        let body = self.body_from_py(body)?;
        py.detach(|| {
            let mut log = self.lock()?;
            log.append(&body).map_err(map_error)
        })
    }

    /// Append one record and fsync the block holding it before returning.
    fn append_durable(&self, py: Python<'_>, body: &Bound<'_, PyDict>) -> PyResult<u64> {
        let body = self.body_from_py(body)?;
        py.detach(|| {
            let mut log = self.lock()?;
            log.append_durable(&body).map_err(map_error)
        })
    }

    /// Append a batch of bodies. Returns the offsets, in the order given.
    fn append_many(&self, py: Python<'_>, records: &Bound<'_, PyList>) -> PyResult<Vec<u64>> {
        let mut prepared = Vec::with_capacity(records.len());
        for item in records.iter() {
            let body: Bound<'_, PyDict> = item.extract()?;
            prepared.push(self.body_from_py(&body)?);
        }
        py.detach(|| {
            let mut log = self.lock()?;
            let mut offsets = Vec::with_capacity(prepared.len());
            for body in &prepared {
                offsets.push(log.append(body).map_err(map_error)?);
            }
            Ok(offsets)
        })
    }

    /// Write and fsync every buffered block without appending anything: the durability
    /// boundary for a caller who batches.
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            let mut log = self.lock()?;
            log.flush().map_err(map_error)
        })
    }

    /// Seal the open parts (footer, fsync, rename). Returns their totals.
    fn seal(&self, py: Python<'_>) -> PyResult<Vec<(u64, u64, u32, u64)>> {
        py.detach(|| {
            let mut log = self.lock()?;
            let footers = log.seal().map_err(map_error)?;
            Ok(footers
                .into_iter()
                .map(|footer| (footer.records, footer.last_ts, footer.blocks, footer.digest))
                .collect())
        })
    }

    /// Read up to `max` records starting at `from_offset`. With a partitioned log the
    /// offset applies to every stream and rows come back in time order.
    #[pyo3(signature = (from_offset = 0, max = 100))]
    fn read(&self, py: Python<'_>, from_offset: u64, max: usize) -> PyResult<Vec<Py<Record>>> {
        let rows = py.detach(|| {
            let log = self.lock()?;
            log.read(from_offset, max).map_err(map_error)
        })?;
        rows.into_iter()
            .map(|(ts, offset, body)| self.record_from(ts, offset, &body))
            .collect()
    }

    /// Read every record whose timestamp is in `[from_ts, to_ts]`, up to `max`.
    #[pyo3(signature = (from_ts, to_ts, max = 100))]
    fn read_by_time(
        &self,
        py: Python<'_>,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> PyResult<Vec<Py<Record>>> {
        let rows = py.detach(|| {
            let log = self.lock()?;
            log.read_by_time(from_ts, to_ts, max).map_err(map_error)
        })?;
        rows.into_iter()
            .map(|(ts, offset, body)| self.record_from(ts, offset, &body))
            .collect()
    }

    /// Drop every bucket older than the one `ts` falls in. Returns the bucket numbers
    /// that went away; no tombstones are written, a directory is removed.
    fn retain_before(&self, py: Python<'_>, ts: u64) -> PyResult<Vec<u64>> {
        py.detach(|| {
            let mut log = self.lock()?;
            log.retain_before(ts).map_err(map_error)
        })
    }

    /// The physical summary of every part: what a reader can know without opening them.
    fn parts(&self, py: Python<'_>) -> PyResult<Vec<Py<PyDict>>> {
        let parts = py.detach(|| {
            let log = self.lock()?;
            log.parts().map_err(map_error)
        })?;
        parts
            .into_iter()
            .map(|part| {
                let dict = PyDict::new(py);
                dict.set_item("bucket", part.bucket)?;
                dict.set_item("partition", part.partition)?;
                dict.set_item("path", part.path.to_string_lossy().to_string())?;
                dict.set_item("sealed", part.sealed)?;
                dict.set_item("first_offset", part.first_offset)?;
                dict.set_item("records", part.records)?;
                dict.set_item("first_ts", part.first_ts)?;
                dict.set_item("last_ts", part.last_ts)?;
                dict.set_item("bytes", part.bytes)?;
                Ok(dict.into())
            })
            .collect()
    }

    /// What `log.meta` says: identity, the same for every process that opens the log.
    #[getter]
    fn meta(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let log = self.lock()?;
        let meta = log.meta();
        let dict = PyDict::new(py);
        dict.set_item("epoch_unix_ms", meta.epoch_unix_ms)?;
        dict.set_item("precision", precision_label(meta.precision))?;
        dict.set_item("bucket_seconds", meta.bucket_seconds)?;
        dict.set_item("partition_tag", meta.partition_tag)?;
        Ok(dict.into())
    }

    #[getter]
    fn root(&self) -> PyResult<String> {
        let log = self.lock()?;
        Ok(log.root().to_string_lossy().to_string())
    }

    fn __repr__(&self) -> PyResult<String> {
        let log = self.lock()?;
        Ok(format!(
            "Log(root={:?}, precision={}, bucket_seconds={}, partition_tag={:?})",
            log.root().to_string_lossy(),
            precision_label(log.meta().precision),
            log.meta().bucket_seconds,
            log.meta().partition_tag
        ))
    }
}

impl PyLog {
    fn lock(&self) -> PyResult<std::sync::MutexGuard<'_, Log>> {
        self.inner
            .lock()
            .map_err(|_| PyValueError::new_err("the log lock was poisoned by a failed call"))
    }

    /// A Python dict → a body. Keys are tags (ints) or names, if the log knows them.
    fn body_from_py(&self, body: &Bound<'_, PyDict>) -> PyResult<Body> {
        let mut out = Body::new();
        for (key, value) in body.iter() {
            let tag = self.tag_from_py(&key)?;
            out.push(tag, self.value_from_py(&value)?);
        }
        Ok(out)
    }

    fn tag_from_py(&self, key: &Bound<'_, PyAny>) -> PyResult<u16> {
        if let Ok(tag) = key.extract::<u16>() {
            return Ok(tag);
        }
        if let Ok(name) = key.extract::<String>() {
            return match self.tags.get(&name) {
                Some(tag) => Ok(*tag),
                None => Err(PyValueError::new_err(format!(
                    "unknown field name {name:?}: pass `tags={{...}}` to Log() to name fields"
                ))),
            };
        }
        Err(PyValueError::new_err(
            "body keys must be int tags (0..65535) or names given to Log(tags=...)",
        ))
    }

    /// A Python value → an nTLV value. Nested lists and dicts are refused rather than
    /// flattened: the body is flat by construction, and guessing would be a silent loss
    /// of structure.
    fn value_from_py(&self, value: &Bound<'_, PyAny>) -> PyResult<Value> {
        if value.is_none() {
            return Err(PyValueError::new_err(
                "None is not a value: leave the tag out of the dict instead",
            ));
        }
        if let Ok(text) = value.extract::<String>() {
            return Ok(Value::Str(text));
        }
        if let Ok(bytes) = value.cast::<PyBytes>() {
            return Ok(Value::Bytes(bytes.as_bytes().to_vec()));
        }
        if value.is_instance_of::<pyo3::types::PyBool>() {
            return Ok(Value::U64(u64::from(value.extract::<bool>()?)));
        }
        if let Ok(number) = value.extract::<u64>() {
            return Ok(Value::U64(number));
        }
        if let Ok(number) = value.extract::<i64>() {
            return Ok(Value::I64(number));
        }
        if let Ok(number) = value.extract::<f64>() {
            return Ok(Value::F64(number));
        }
        Err(PyValueError::new_err(format!(
            "unsupported value type {}: use str, int, float, bool or bytes",
            value.get_type().name()?
        )))
    }

    fn value_to_py<'py>(&self, py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
        Ok(match value {
            Value::Str(text) => text.into_pyobject(py)?.into_any(),
            Value::U64(number) => number.into_pyobject(py)?.into_any(),
            Value::I64(number) => number.into_pyobject(py)?.into_any(),
            Value::F64(number) => number.into_pyobject(py)?.into_any(),
            Value::Bytes(bytes) => PyBytes::new(py, bytes).into_any(),
        })
    }

    fn record_from(&self, ts: u64, offset: u64, body: &Body) -> PyResult<Py<Record>> {
        Python::attach(|py| {
            let dict = PyDict::new(py);
            // The write time under a name of its own, unless the body already claimed
            // that name (then the caller's value stays: their data is theirs).
            if let Some(name) = self.ts_field.as_deref() {
                if !dict.contains(name)? {
                    dict.set_item(name, ts)?;
                }
            }
            for (tag, value) in body.items() {
                let key: Bound<'_, PyAny> = match self.names.get(tag) {
                    Some(name) => name.into_pyobject(py)?.into_any(),
                    None => tag.into_pyobject(py)?.into_any(),
                };
                dict.set_item(key, self.value_to_py(py, value)?)?;
            }
            Py::new(
                py,
                Record {
                    ts,
                    offset,
                    body: dict.into(),
                },
            )
        })
    }
}

#[pymodule]
#[pyo3(name = "okm_series")]
pub fn okm_series_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyLog>()?;
    module.add_class::<Record>()?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
