#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The binding as a Python caller sees it: the module is built in-process and driven
//! through attribute lookups and calls, so nothing here depends on a Rust-side shortcut.

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule};

fn module(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    let module = PyModule::new(py, "okm_series")?;
    okm_series_py::okm_series_module(&module)?;
    Ok(module)
}

fn open_log<'py>(
    py: Python<'py>,
    module: &Bound<'py, PyModule>,
    root: &std::path::Path,
    extra: &[(&str, Bound<'py, PyAny>)],
) -> PyResult<Bound<'py, PyAny>> {
    let kwargs = PyDict::new(py);
    for (key, value) in extra {
        kwargs.set_item(*key, value)?;
    }
    module
        .getattr("Log")?
        .call((root.to_str().unwrap(),), Some(&kwargs))
}

#[test]
fn a_python_caller_appends_reads_and_keeps_its_types() {
    Python::attach(|py| -> PyResult<()> {
        let dir = tempfile::tempdir().unwrap();
        let module = module(py)?;
        let log = open_log(py, &module, dir.path(), &[])?;

        let body = PyDict::new(py);
        body.set_item(1u16, "error")?;
        body.set_item(2u16, 42u64)?;
        body.set_item(3u16, -7i64)?;
        body.set_item(4u16, 0.5f64)?;
        body.set_item(5u16, pyo3::types::PyBytes::new(py, b"raw"))?;
        let offset: u64 = log.call_method1("append", (body,))?.extract()?;
        assert_eq!(offset, 0);
        log.call_method0("flush")?;

        let records = log.call_method1("read", (0u64, 100usize))?;
        let list = records.cast::<PyList>()?;
        assert_eq!(list.len(), 1);
        let record = list.get_item(0)?;
        let ts: u64 = record.getattr("ts")?.extract()?;
        assert!(ts > 0, "the log stamped the record at write time");
        assert_eq!(record.getattr("offset")?.extract::<u64>()?, 0);
        let read_back: Bound<'_, PyDict> = record.getattr("body")?.cast_into()?;
        assert_eq!(
            read_back.get_item(1)?.unwrap().extract::<String>()?,
            "error"
        );
        assert_eq!(read_back.get_item(2)?.unwrap().extract::<u64>()?, 42);
        assert_eq!(read_back.get_item(3)?.unwrap().extract::<i64>()?, -7);
        assert_eq!(read_back.get_item(4)?.unwrap().extract::<f64>()?, 0.5);
        assert_eq!(
            read_back.get_item(5)?.unwrap().extract::<Vec<u8>>()?,
            b"raw".to_vec()
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn named_tags_work_when_the_log_is_opened_with_them() {
    Python::attach(|py| -> PyResult<()> {
        let dir = tempfile::tempdir().unwrap();
        let module = module(py)?;
        let tags = PyDict::new(py);
        tags.set_item("level", 1u16)?;
        tags.set_item("message", 2u16)?;
        let log = open_log(py, &module, dir.path(), &[("tags", tags.into_any())])?;

        let body = PyDict::new(py);
        body.set_item("level", "warn")?;
        body.set_item("message", "disk almost full")?;
        log.call_method1("append", (body,))?;
        log.call_method0("flush")?;

        let records = log.call_method1("read", (0u64, 10usize))?;
        let list = records.cast::<PyList>()?;
        let record = list.get_item(0)?;
        let read_back: Bound<'_, PyDict> = record.getattr("body")?.cast_into()?;
        assert_eq!(
            read_back
                .get_item("message")?
                .unwrap()
                .extract::<String>()?,
            "disk almost full"
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_reader_can_ask_for_one_stream() {
    Python::attach(|py| -> PyResult<()> {
        let dir = tempfile::tempdir().unwrap();
        let module = module(py)?;
        let log = open_log(
            py,
            &module,
            dir.path(),
            &[("partition_tag", 9u16.into_pyobject(py)?.into_any())],
        )?;

        for (tenant, index) in [("eu", 0u64), ("us", 0), ("eu", 1), ("us", 1)] {
            let body = PyDict::new(py);
            body.set_item(1u16, index)?;
            body.set_item(9u16, tenant)?;
            log.call_method1("append", (body,))?;
        }
        log.call_method0("flush")?;

        let eu = log.call_method1("read_stream", ("eu", 0u64, 100usize))?;
        let eu = eu.cast::<PyList>()?;
        assert_eq!(eu.len(), 2, "the eu stream only");
        let offsets: Vec<u64> = (0..eu.len())
            .map(|index| {
                eu.get_item(index)
                    .unwrap()
                    .getattr("offset")
                    .unwrap()
                    .extract()
                    .unwrap()
            })
            .collect();
        assert_eq!(offsets, vec![0, 1], "offsets count per stream");
        let body: Bound<'_, PyDict> = eu.get_item(0)?.getattr("body")?.cast_into()?;
        assert_eq!(
            body.get_item(9)?.unwrap().extract::<String>()?,
            "eu",
            "no other stream leaks into the answer"
        );

        let all = log.call_method1("read", (0u64, 100usize))?;
        assert_eq!(all.cast::<PyList>()?.len(), 4, "read is still the union");
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_partitioned_log_needs_the_field() {
    Python::attach(|py| -> PyResult<()> {
        let dir = tempfile::tempdir().unwrap();
        let module = module(py)?;
        let log = open_log(
            py,
            &module,
            dir.path(),
            &[("partition_tag", 9u16.into_pyobject(py)?.into_any())],
        )?;

        let missing = PyDict::new(py);
        missing.set_item(1u16, "no partition")?;
        let refused = log.call_method1("append", (missing,));
        let error = refused.expect_err("a partitioned log refuses a record without the tag");
        assert!(error.to_string().contains("partition"), "{}", error);

        let with_partition = PyDict::new(py);
        with_partition.set_item(1u16, "row")?;
        with_partition.set_item(9u16, "eu")?;
        log.call_method1("append", (with_partition,))?;
        log.call_method0("flush")?;

        let parts = log.call_method0("parts")?;
        let list = parts.cast::<PyList>()?;
        assert_eq!(list.len(), 1);
        let part: Bound<'_, PyDict> = list.get_item(0)?.cast_into()?;
        assert_eq!(
            part.get_item("partition")?.unwrap().extract::<String>()?,
            "eu"
        );
        assert_eq!(part.get_item("records")?.unwrap().extract::<u64>()?, 1);
        Ok(())
    })
    .unwrap();
}

#[test]
fn the_write_time_is_the_logs_and_shows_up_under_its_own_name() {
    Python::attach(|py| -> PyResult<()> {
        let dir = tempfile::tempdir().unwrap();
        let module = module(py)?;
        let log = open_log(
            py,
            &module,
            dir.path(),
            &[("precision", "ms".into_pyobject(py)?.into_any())],
        )?;

        // No timestamp argument exists: the caller appends a body, the log stamps it.
        let body = PyDict::new(py);
        body.set_item(1u16, "first")?;
        log.call_method1("append", (body,))?;
        let body2 = PyDict::new(py);
        body2.set_item(1u16, "second")?;
        log.call_method1("append", (body2,))?;
        log.call_method0("flush")?;

        let records = log.call_method1("read", (0u64, 10usize))?;
        let list = records.cast::<PyList>()?;
        assert_eq!(list.len(), 2);
        let first = list.get_item(0)?;
        let second = list.get_item(1)?;
        let ts_first: u64 = first.getattr("ts")?.extract()?;
        let ts_second: u64 = second.getattr("ts")?.extract()?;
        assert!(ts_second >= ts_first, "the log's time never goes backwards");

        // The record's own time is readable as a field, next to the caller's data.
        let body_first: Bound<'_, PyDict> = first.getattr("body")?.cast_into()?;
        assert_eq!(
            body_first.get_item("_ts")?.unwrap().extract::<u64>()?,
            ts_first
        );
        assert_eq!(
            body_first.get_item(1)?.unwrap().extract::<String>()?,
            "first"
        );

        let meta: Bound<'_, PyDict> = log.getattr("meta")?.cast_into()?;
        assert_eq!(
            meta.get_item("precision")?.unwrap().extract::<String>()?,
            "ms"
        );

        // The name is configurable, and None turns the extra field off.
        let renamed = open_log(
            py,
            &module,
            &dir.path().join("renamed"),
            &[
                ("precision", "ms".into_pyobject(py)?.into_any()),
                ("ts_field", "written_at".into_pyobject(py)?.into_any()),
            ],
        )?;
        let body3 = PyDict::new(py);
        body3.set_item(1u16, "row")?;
        renamed.call_method1("append", (body3,))?;
        renamed.call_method0("flush")?;
        let rows = renamed.call_method1("read", (0u64, 10usize))?;
        let rows = rows.cast::<PyList>()?;
        let only: Bound<'_, PyDict> = rows.get_item(0)?.getattr("body")?.cast_into()?;
        assert!(only.get_item("written_at")?.is_some());
        assert!(only.get_item("_ts")?.is_none());

        let bare = open_log(
            py,
            &module,
            &dir.path().join("bare"),
            &[
                ("precision", "ms".into_pyobject(py)?.into_any()),
                ("ts_field", py.None().into_bound(py)),
            ],
        )?;
        let body4 = PyDict::new(py);
        body4.set_item(1u16, "row")?;
        bare.call_method1("append", (body4,))?;
        bare.call_method0("flush")?;
        let rows = bare.call_method1("read", (0u64, 10usize))?;
        let rows = rows.cast::<PyList>()?;
        let only: Bound<'_, PyDict> = rows.get_item(0)?.getattr("body")?.cast_into()?;
        assert!(
            only.get_item("_ts")?.is_none(),
            "ts_field=None adds nothing"
        );
        Ok(())
    })
    .unwrap();
}
