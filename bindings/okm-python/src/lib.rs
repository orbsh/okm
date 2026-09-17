//! PyO3 bindings for OKM's dynamic codec (PLAN Phase 5, dynamic codec
//! "Python first"). The Python side consumes the structured schema
//! export (`TableSchema`, serde JSON) and the value tree mirrors
//! `okm_dynamic::Value` — no derive, no Rust compile step for readers.
//!
//! Surface:
//! - `Schema.from_json(str)` — parse a serde'd TableSchema
//! - `Schema.encode_key(dict) / encode_payload(dict) -> bytes`
//! - `Schema.decode_key(bytes) / decode_payload(bytes) -> dict`
//! - `Value` mapping: Python int/float/bool/str/bytes/list/dict ↔
//!   okm_dynamic Value (dict = nested obj is NOT valid here — the typed
//!   codec has no nested kind; raises TypeError).
//!
//! Embedded-actor use (ADR-0012 ceiling): encode/decode only — no
//! reduce/subscribe, which stay Rust compile-time by design.
use okm_core::schema::TableSchema;
use okm_dynamic::{decode_key, decode_payload, encode_key, encode_payload, Value, ValueMap};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use std::collections::BTreeMap;
use std::sync::Arc;

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
    })
}

fn map_from_py(schema: &TableSchema, values: &Bound<'_, PyAny>) -> PyResult<ValueMap> {
    let dict = values
        .downcast::<pyo3::types::PyDict>()
        .map_err(|_| PyTypeError::new_err("values must be a dict {field: value}"))?;
    let mut out = BTreeMap::new();
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

fn value_to_py(v: &Value) -> PyObject {
    use pyo3::conversion::IntoPyObject;
    Python::with_gil(|py| match v {
        Value::U8(x) => (*x).into_pyobject(py).unwrap().unbind().into_any(),
        Value::U16(x) => (*x).into_pyobject(py).unwrap().unbind().into_any(),
        Value::U32(x) => (*x).into_pyobject(py).unwrap().unbind().into_any(),
        Value::U64(x) => (*x).into_pyobject(py).unwrap().unbind().into_any(),
        Value::I64(x) => (*x).into_pyobject(py).unwrap().unbind().into_any(),
        Value::F64(x) => (*x).into_pyobject(py).unwrap().unbind().into_any(),
        Value::Bool(x) => (*x).into_pyobject(py).unwrap().to_owned().unbind().into_any(),
        Value::Null => py.None(),
        Value::Str(s) => s.into_pyobject(py).unwrap().unbind().into_any(),
        Value::Bytes(b) => b.into_pyobject(py).unwrap().unbind().into_any(),
    })
}

fn map_to_py(m: &ValueMap) -> PyObject {
    Python::with_gil(|py| {
        let d = pyo3::types::PyDict::new(py);
        for (k, v) in m {
            d.set_item(k, value_to_py(v)).ok();
        }
        d.into_any().unbind()
    })
}

fn codec_err(e: okm_dynamic::CodecError) -> PyErr {
    PyValueError::new_err(format!("{e}"))
}

/// A parsed OKM table schema. Build from `Table::json_schema()` output or
/// any serde'd TableSchema JSON.
#[pyclass]
struct Schema {
    inner: Arc<TableSchema>,
}

#[pymethods]
impl Schema {
    #[new]
    fn from_json(json: &str) -> PyResult<Self> {
        let schema: TableSchema = serde_json::from_str(json)
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
        encode_key(&*self.inner, &map_from_py(&*self.inner, values)?).map_err(codec_err)
    }

    /// Encode the payload (hot segment + cold TLV) from `{field: value}`.
    fn encode_payload(&self, values: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
        encode_payload(&*self.inner, &map_from_py(&*self.inner, values)?).map_err(codec_err)
    }

    /// Decode key bytes → `{field: value}`.
    fn decode_key(&self, bytes: &[u8]) -> PyResult<PyObject> {
        decode_key(&self.inner, bytes).map(|m| map_to_py(&m)).map_err(codec_err)
    }

    /// Decode payload bytes → `{field: value}`. Absent tail fields arrive
    /// as their schema defaults (version migration).
    fn decode_payload(&self, bytes: &[u8]) -> PyResult<PyObject> {
        decode_payload(&self.inner, bytes)
            .map(|m| map_to_py(&m))
            .map_err(codec_err)
    }
}

/// okm — OKM dynamic codec for Python.
#[pymodule]
fn okm(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Schema>()?;
    Ok(())
}
