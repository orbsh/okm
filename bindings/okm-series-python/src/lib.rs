//! PyO3 bridge for `okm-series` (ADR-0046).
//!
//! What a Python caller gets is the crate's own shape, not a wrapper with opinions:
//! a `Log` whose `append` returns an offset, `read` that starts at an offset,
//! `read_by_time` for a window, `seal`, `retain_before`, and `parts` for the physical
//! summary. `Reader` is the same log opened for reading only — no `LOCK`, no claim — so a
//! Python process can read a log another process is writing. `iter_records(partition, batch)`
//! on either class pages one stream without materialising it.
//!
//! Bodies are plain dicts keyed by tag (or by name, if the log was opened with
//! a `tags` map), and values are `str` / `int` / `float` / `bytes`.
//!
//! Errors cross as Python exceptions: an IO failure is `OSError`, everything else a
//! refusal (time went backwards, a missing partition field, a layout mismatch) is
//! `ValueError` — the caller can act on all of them.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ::okm_series::{
    Body, Codec, Config, DictConfig, Log, LogReader, Meta, PartInfo, Precision, Value,
};
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
        rows(
            py,
            &self.inner,
            &self.names,
            self.ts_field.as_deref(),
            |log| log.read_rows(from_offset, max),
        )
    }

    /// Read one stream: the records under `partition`, or — with `None` — the single stream
    /// of an unpartitioned log. Use the value `parts()` reports (`"eu"`, `"7"`, hex bytes);
    /// `read` is the union of every stream.
    #[pyo3(signature = (partition, from_offset = 0, max = 100))]
    fn read_stream(
        &self,
        py: Python<'_>,
        partition: Option<&str>,
        from_offset: u64,
        max: usize,
    ) -> PyResult<Vec<Py<Record>>> {
        rows(
            py,
            &self.inner,
            &self.names,
            self.ts_field.as_deref(),
            |log| log.read_stream_rows(partition, from_offset, max),
        )
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
        rows(
            py,
            &self.inner,
            &self.names,
            self.ts_field.as_deref(),
            |log| log.read_by_time_rows(from_ts, to_ts, max),
        )
    }

    /// Read one stream's records whose timestamp is in `[from_ts, to_ts]`, up to `max`.
    #[pyo3(signature = (partition, from_ts, to_ts, max = 100))]
    fn read_stream_by_time(
        &self,
        py: Python<'_>,
        partition: Option<&str>,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> PyResult<Vec<Py<Record>>> {
        rows(
            py,
            &self.inner,
            &self.names,
            self.ts_field.as_deref(),
            |log| log.read_stream_by_time_rows(partition, from_ts, to_ts, max),
        )
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
        part_dicts(py, &self.inner)
    }

    /// What `log.meta` says: identity, the same for every process that opens the log.
    #[getter]
    fn meta(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        meta_dict(py, &self.inner)
    }

    /// Iterate one stream, `batch` records per read, without materialising the log.
    ///
    /// A cursor walks ONE stream: offsets are a stream's own counter, so the union's rows carry
    /// several counters interleaved in time order and a single "next offset" would skip records.
    /// An unpartitioned log has one stream (`iter_records()` is the whole log); with partitions,
    /// iterate the values `parts()` reports.
    #[pyo3(signature = (partition = None, batch = 1000))]
    fn iter_records(
        slf: PyRef<'_, Self>,
        py: Python<'_>,
        partition: Option<&str>,
        batch: usize,
    ) -> PyResult<Py<Records>> {
        let source = slf.into_pyobject(py)?.into_any().unbind();
        Py::new(py, Records::new(source, partition, batch))
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
}

/// One record as Python sees it: the write time under a name of its own (unless the body
/// already claimed that name — the caller's value stays, their data is theirs), then the body
/// with tag names where the opener knew them.
fn record_of(
    py: Python<'_>,
    names: &HashMap<u16, String>,
    ts_field: Option<&str>,
    ts: u64,
    offset: u64,
    body: &Body,
) -> PyResult<Py<Record>> {
    let dict = PyDict::new(py);
    if let Some(name) = ts_field {
        if !dict.contains(name)? {
            dict.set_item(name, ts)?;
        }
    }
    for (tag, value) in body.items() {
        let key: Bound<'_, PyAny> = match names.get(tag) {
            Some(name) => name.into_pyobject(py)?.into_any(),
            None => tag.into_pyobject(py)?.into_any(),
        };
        dict.set_item(key, value_to_py(py, value)?)?;
    }
    Py::new(
        py,
        Record {
            ts,
            offset,
            body: dict.into(),
        },
    )
}

fn value_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Str(text) => text.into_pyobject(py)?.into_any(),
        Value::U64(number) => number.into_pyobject(py)?.into_any(),
        Value::I64(number) => number.into_pyobject(py)?.into_any(),
        Value::F64(number) => number.into_pyobject(py)?.into_any(),
        Value::Bytes(bytes) => PyBytes::new(py, bytes).into_any(),
    })
}

/// The reading surface, implemented by both classes: `Log` holds a writer and a reader,
/// `Reader` holds only the reader. Writing the six methods twice would be two copies of one
/// behaviour, so the classes' `#[pymethods]` bodies are one line each and the work lives here.
trait ReadFace {
    fn read_rows(
        &self,
        from: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error>;
    fn read_stream_rows(
        &self,
        partition: Option<&str>,
        from: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error>;
    fn read_by_time_rows(
        &self,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error>;
    fn read_stream_by_time_rows(
        &self,
        partition: Option<&str>,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error>;
    fn part_infos(&self) -> Result<Vec<PartInfo>, ::okm_series::Error>;
    fn meta(&self) -> &Meta;
    fn root_path(&self) -> &Path;
}

impl ReadFace for Log {
    fn read_rows(
        &self,
        from: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> {
        self.read(from, max)
    }
    fn read_stream_rows(
        &self,
        partition: Option<&str>,
        from: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> {
        self.read_stream(partition, from, max)
    }
    fn read_by_time_rows(
        &self,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> {
        self.read_by_time(from_ts, to_ts, max)
    }
    fn read_stream_by_time_rows(
        &self,
        partition: Option<&str>,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> {
        self.read_stream_by_time(partition, from_ts, to_ts, max)
    }
    fn part_infos(&self) -> Result<Vec<PartInfo>, ::okm_series::Error> {
        self.parts()
    }
    fn meta(&self) -> &Meta {
        Log::meta(self)
    }
    fn root_path(&self) -> &Path {
        Log::root(self)
    }
}

impl ReadFace for LogReader {
    fn read_rows(
        &self,
        from: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> {
        self.read(from, max)
    }
    fn read_stream_rows(
        &self,
        partition: Option<&str>,
        from: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> {
        self.read_stream(partition, from, max)
    }
    fn read_by_time_rows(
        &self,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> {
        self.read_by_time(from_ts, to_ts, max)
    }
    fn read_stream_by_time_rows(
        &self,
        partition: Option<&str>,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> {
        self.read_stream_by_time(partition, from_ts, to_ts, max)
    }
    fn part_infos(&self) -> Result<Vec<PartInfo>, ::okm_series::Error> {
        self.parts()
    }
    fn meta(&self) -> &Meta {
        LogReader::meta(self)
    }
    fn root_path(&self) -> &Path {
        LogReader::root(self)
    }
}

/// Lock, read without the GIL, render. One of these per method shape; the classes' bodies are
/// one call each, so `Log` and `Reader` cannot drift apart.
fn rows<F: ReadFace + Send>(
    py: Python<'_>,
    face: &Mutex<F>,
    names: &HashMap<u16, String>,
    ts_field: Option<&str>,
    run: impl FnOnce(&F) -> Result<Vec<(u64, u64, Body)>, ::okm_series::Error> + Send,
) -> PyResult<Vec<Py<Record>>> {
    let rows = py.detach(|| {
        let face = face
            .lock()
            .map_err(|_| PyValueError::new_err("the log lock was poisoned by a failed call"))?;
        run(&face).map_err(map_error)
    })?;
    rows.into_iter()
        .map(|(ts, offset, body)| record_of(py, names, ts_field, ts, offset, &body))
        .collect()
}

fn part_dicts<F: ReadFace + Send>(py: Python<'_>, face: &Mutex<F>) -> PyResult<Vec<Py<PyDict>>> {
    let parts = py.detach(|| {
        let face = face
            .lock()
            .map_err(|_| PyValueError::new_err("the log lock was poisoned by a failed call"))?;
        face.part_infos().map_err(map_error)
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

fn meta_dict<F: ReadFace + Send>(py: Python<'_>, face: &Mutex<F>) -> PyResult<Py<PyDict>> {
    let face = face
        .lock()
        .map_err(|_| PyValueError::new_err("the log lock was poisoned by a failed call"))?;
    let meta = face.meta();
    let dict = PyDict::new(py);
    dict.set_item("epoch_unix_ms", meta.epoch_unix_ms)?;
    dict.set_item("precision", precision_label(meta.precision))?;
    dict.set_item("bucket_seconds", meta.bucket_seconds)?;
    dict.set_item("partition_tag", meta.partition_tag)?;
    Ok(dict.into())
}

/// The same log, opened for READING only.
///
/// No `LOCK` is taken and no claim is registered, so a Python process can read a log that
/// another process is writing, and a thread can read while the writer in this process holds its
/// handle. What it sees is what is on disk: a record appended but not yet flushed is not there,
/// and reading again — the reader caches each part by its file length — is how you see it later.
/// A reader cannot create a log; the directory must already be one.
#[pyclass(name = "Reader", module = "okm_series")]
pub struct PyReader {
    inner: Mutex<LogReader>,
    names: HashMap<u16, String>,
    ts_field: Option<String>,
}

#[pymethods]
impl PyReader {
    #[new]
    #[pyo3(signature = (root, *, precision = "ns", bucket_seconds = 3600,
        partition_tag = None, tags = None, ts_field = Some("_ts".to_string())))]
    fn new(
        root: PathBuf,
        precision: &str,
        bucket_seconds: u64,
        partition_tag: Option<u16>,
        tags: Option<HashMap<String, u16>>,
        ts_field: Option<String>,
    ) -> PyResult<Self> {
        // Block size, codec and the dictionary are the WRITER's: each part's header says which
        // codec its blocks use and `part-<n>.dict` beside it says whether codes are in play, so
        // a reader that was told none of it still reads everything.
        let config = Config {
            precision: precision_of(precision)?,
            bucket_seconds,
            partition_tag,
            ..Config::default()
        };
        let reader = LogReader::open(&root, config).map_err(map_error)?;
        let mut names = HashMap::new();
        for (name, tag) in tags.unwrap_or_default() {
            names.insert(tag, name);
        }
        Ok(PyReader {
            inner: Mutex::new(reader),
            names,
            ts_field,
        })
    }

    /// Read up to `max` records starting at `from_offset`, across every stream, in time order.
    #[pyo3(signature = (from_offset = 0, max = 100))]
    fn read(&self, py: Python<'_>, from_offset: u64, max: usize) -> PyResult<Vec<Py<Record>>> {
        rows(
            py,
            &self.inner,
            &self.names,
            self.ts_field.as_deref(),
            |reader| reader.read_rows(from_offset, max),
        )
    }

    /// Read one stream: the records under `partition`, or — with `None` — the single stream of an
    /// unpartitioned log. Use the value `parts()` reports; `read` is the union of every stream.
    #[pyo3(signature = (partition, from_offset = 0, max = 100))]
    fn read_stream(
        &self,
        py: Python<'_>,
        partition: Option<&str>,
        from_offset: u64,
        max: usize,
    ) -> PyResult<Vec<Py<Record>>> {
        rows(
            py,
            &self.inner,
            &self.names,
            self.ts_field.as_deref(),
            |reader| reader.read_stream_rows(partition, from_offset, max),
        )
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
        rows(
            py,
            &self.inner,
            &self.names,
            self.ts_field.as_deref(),
            |reader| reader.read_by_time_rows(from_ts, to_ts, max),
        )
    }

    /// Read one stream's records whose timestamp is in `[from_ts, to_ts]`, up to `max`.
    #[pyo3(signature = (partition, from_ts, to_ts, max = 100))]
    fn read_stream_by_time(
        &self,
        py: Python<'_>,
        partition: Option<&str>,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> PyResult<Vec<Py<Record>>> {
        rows(
            py,
            &self.inner,
            &self.names,
            self.ts_field.as_deref(),
            |reader| reader.read_stream_by_time_rows(partition, from_ts, to_ts, max),
        )
    }

    /// The physical summary of every part: what a reader can know without opening them.
    fn parts(&self, py: Python<'_>) -> PyResult<Vec<Py<PyDict>>> {
        part_dicts(py, &self.inner)
    }

    /// What `log.meta` says: identity, the same for every process that opens the log.
    #[getter]
    fn meta(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        meta_dict(py, &self.inner)
    }

    #[getter]
    fn root(&self) -> PyResult<String> {
        let reader = self.lock()?;
        Ok(reader.root_path().to_string_lossy().to_string())
    }

    /// Iterate one stream, `batch` records per read. See `Log.iter_records` for why a cursor
    /// walks one stream rather than the union.
    #[pyo3(signature = (partition = None, batch = 1000))]
    fn iter_records(
        slf: PyRef<'_, Self>,
        py: Python<'_>,
        partition: Option<&str>,
        batch: usize,
    ) -> PyResult<Py<Records>> {
        let source = slf.into_pyobject(py)?.into_any().unbind();
        Py::new(py, Records::new(source, partition, batch))
    }

    fn __repr__(&self) -> PyResult<String> {
        let reader = self.lock()?;
        let meta = reader.meta();
        Ok(format!(
            "Reader(root={:?}, precision={}, bucket_seconds={}, partition_tag={:?})",
            reader.root().to_string_lossy(),
            precision_label(meta.precision),
            meta.bucket_seconds,
            meta.partition_tag
        ))
    }
}

impl PyReader {
    fn lock(&self) -> PyResult<std::sync::MutexGuard<'_, LogReader>> {
        self.inner
            .lock()
            .map_err(|_| PyValueError::new_err("the reader lock was poisoned by a failed call"))
    }
}

/// One stream, paged: each `next()` asks its source for `batch` records from where the previous
/// call stopped.
///
/// A cursor walks ONE stream, not the union. Offsets are a stream's own counter (ADR-0046 §9),
/// so `read(from, max)` over the union returns several counters interleaved in time order and
/// "the next offset" is not a single number — paging it would skip records. An unpartitioned log
/// has exactly one stream, so `iter_records()` is the whole log; a partitioned one is iterated
/// per partition value, which is what `parts()` reports.
#[pyclass(name = "Records", module = "okm_series")]
pub struct Records {
    /// The `Log` or `Reader` that answers `read_stream`. Held strongly: a cursor outliving the
    /// log it reads is a use-after-close, not a convenience.
    source: Py<PyAny>,
    partition: Option<String>,
    batch: usize,
    next_offset: u64,
    buffer: VecDeque<Py<Record>>,
    done: bool,
}

impl Records {
    fn new(source: Py<PyAny>, partition: Option<&str>, batch: usize) -> Self {
        Records {
            source,
            partition: partition.map(str::to_string),
            batch: batch.max(1),
            next_offset: 0,
            buffer: VecDeque::new(),
            done: false,
        }
    }
}

#[pymethods]
impl Records {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(mut slf: PyRefMut<'_, Self>) -> PyResult<Option<Py<Record>>> {
        if slf.buffer.is_empty() && !slf.done {
            let py = slf.py();
            let read = slf.source.bind(py).getattr("read_stream")?;
            let batch = read.call1((slf.partition.clone(), slf.next_offset, slf.batch))?;
            let fetched: Vec<Py<Record>> = batch.extract()?;
            match fetched.last() {
                None => slf.done = true,
                Some(last) => {
                    slf.next_offset = last.borrow(py).offset.saturating_add(1);
                    slf.buffer.extend(fetched);
                }
            }
        }
        Ok(slf.buffer.pop_front())
    }

    fn __repr__(&self) -> String {
        format!(
            "Records(partition={:?}, batch={}, next_offset={}, done={})",
            self.partition, self.batch, self.next_offset, self.done
        )
    }
}

#[pymodule]
#[pyo3(name = "okm_series")]
pub fn okm_series_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyLog>()?;
    module.add_class::<PyReader>()?;
    module.add_class::<Record>()?;
    module.add_class::<Records>()?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
