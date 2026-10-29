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
        let offset: u64 = log
            .call_method1("append", (1_700_000_000_000u64, body))?
            .extract()?;
        assert_eq!(offset, 0);
        log.call_method0("flush")?;

        let records = log.call_method1("read", (0u64, 100usize))?;
        let list = records.cast::<PyList>()?;
        assert_eq!(list.len(), 1);
        let record = list.get_item(0)?;
        assert_eq!(record.getattr("ts")?.extract::<u64>()?, 1_700_000_000_000);
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
        log.call_method1("append", (10u64, body))?;
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
        let refused = log.call_method1("append", (10u64, missing));
        let error = refused.expect_err("a partitioned log refuses a record without the tag");
        assert!(error.to_string().contains("partition"), "{}", error);

        let with_partition = PyDict::new(py);
        with_partition.set_item(1u16, "row")?;
        with_partition.set_item(9u16, "eu")?;
        log.call_method1("append", (10u64, with_partition))?;
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
fn time_backwards_and_retention_come_across_as_python_errors_and_results() {
    Python::attach(|py| -> PyResult<()> {
        let dir = tempfile::tempdir().unwrap();
        let module = module(py)?;
        let log = open_log(
            py,
            &module,
            dir.path(),
            &[("precision", "ms".into_pyobject(py)?.into_any())],
        )?;

        let body = PyDict::new(py);
        body.set_item(1u16, "first")?;
        log.call_method1("append", (7_200_000u64, body))?;
        let body2 = PyDict::new(py);
        body2.set_item(1u16, "earlier")?;
        let refused = log.call_method1("append", (1_000u64, body2));
        assert!(refused.is_err(), "an older record is refused, not clamped");
        log.call_method0("seal")?;

        // A second bucket, then retention drops the older one and reports which.
        let body3 = PyDict::new(py);
        body3.set_item(1u16, "later")?;
        log.call_method1("append", (14_400_000u64, body3))?;
        log.call_method0("seal")?;
        let dropped: Vec<u64> = log
            .call_method1("retain_before", (14_400_000u64,))?
            .extract()?;
        assert_eq!(
            dropped,
            vec![2],
            "the 02:00 bucket goes, the 04:00 one stays"
        );

        let meta: Bound<'_, PyDict> = log.getattr("meta")?.cast_into()?;
        assert_eq!(
            meta.get_item("precision")?.unwrap().extract::<String>()?,
            "ms"
        );
        Ok(())
    })
    .unwrap();
}
