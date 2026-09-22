//! Steel binding for OKM's dynamic codec (PLAN Phase 5 — "Python first,
//! then Steel"). Mirrors the PyO3 surface: a VM receives the schema once
//! (`schema-from-json!`), then encodes/decodes key/payload bytes through
//! the same `okm-dynamic` codec. Values map to native steel types —
//! numbers are exact (steel integers/floats), bytes are bytevectors,
//! strings are strings; the schema kind drives coercion at encode time,
//! same as the PyO3 binding.
//!
//! ADR-0022 scope applies identically: encode/decode plus the
//! callable-implementable semantics (reduce, func/partial indexes via
//! host callables); subscribe stays excluded.
use okm_core::schema::TableSchema;
use okm_dynamic::{decode_key, decode_payload, encode_key, encode_payload, Value, ValueMap};
use steel::{SteelVal, SteelVal::{BoolV, IntV, NumV, StringV}};
use steel::rvals::SteelHashMap;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;
use std::collections::BTreeMap;
use std::sync::Arc;

/// SteelVal → Value, with schema-kind coercion for integers (steel has one
/// integer type; OKM fields are width-strict).
fn steel_to_value(
    name: &str,
    kind: okm_core::FieldType,
    v: &SteelVal,
) -> Result<Value, String> {
    use okm_core::FieldType;
    let bad = |expected: &str| format!("field `{name}`: expected {expected}, got {v:?}");
    Ok(match kind {
        FieldType::Str => match v {
            StringV(s) => Value::Str(s.to_string()),
            other => return Err(bad("string")),
        },
        FieldType::Bytes | FieldType::FixedBytes => {
            // Bytes cross as a scheme vector of integers (0-255) — steel's
            // ByteVector field is private to the crate, a vector is the
            // natural lossless literal.
            let SteelVal::VectorV(vec) = v else {
                return Err(bad("vector of integers 0-255"));
            };
            let mut out = Vec::with_capacity(vec.len());
            for item in vec.iter() {
                let SteelVal::IntV(i) = item else {
                    return Err(bad("vector of integers 0-255"));
                };
                let b = u8::try_from(*i).map_err(|_| bad("vector of integers 0-255"))?;
                out.push(b);
            }
            Value::Bytes(out)
        }
        _ => match v {
            // bool before int: steel BoolV is distinct, no subtype trap.
            BoolV(b) if kind == FieldType::U8 => Value::U8(*b as u8),
            IntV(i) => {
                let i = *i;
                match kind {
                    FieldType::U8 => Value::U8(u8::try_from(i).map_err(|_| bad("u8"))?),
                    FieldType::U16 => Value::U16(u16::try_from(i).map_err(|_| bad("u16"))?),
                    FieldType::U32 => Value::U32(u32::try_from(i).map_err(|_| bad("u32"))?),
                    FieldType::U64 | FieldType::VarInt | FieldType::Quant(_)
                    | FieldType::Offset(_) => {
                        Value::U64(u64::try_from(i).map_err(|_| bad("u64"))?)
                    }
                    FieldType::Enum => Value::U8(u8::try_from(i).map_err(|_| bad("enum tag u8"))?),
                    _ => return Err(bad("numeric kind")),
                }
            }
            NumV(f) if kind == FieldType::U64 || matches!(kind, FieldType::VarInt | FieldType::Quant(_) | FieldType::Offset(_)) => {
                Value::U64(*f as u64)
            }
            BoolV(b) => Value::Bool(*b),
            _ => return Err(bad("numeric")),
        },
    })
}

/// Value → SteelVal.
fn value_to_steel(v: &Value) -> SteelVal {
    match v {
        Value::U8(x) => IntV(*x as isize),
        Value::U16(x) => IntV(*x as isize),
        Value::U32(x) => IntV(*x as isize),
        Value::U64(x) => IntV(*x as isize),
        Value::I64(x) => IntV(*x as isize),
        Value::F64(x) => NumV(*x),
        Value::Bool(x) => BoolV(*x),
        Value::Null => SteelVal::Void,
        Value::Str(s) => SteelVal::StringV(s.as_str().into()),
        Value::Bytes(b) => SteelVal::VectorV(steel::rvals::SteelVector::from(
            steel::gc::Gc::new(b.iter().map(|x| IntV(*x as isize)).collect::<im_rc::Vector<SteelVal>>()),
        )),
    }
}

fn find_field<'a>(
    schema: &'a TableSchema,
    name: &str,
) -> Result<&'a okm_core::schema::FieldSchema, String> {
    schema
        .key_fields
        .iter()
        .chain(schema.hot_fields.iter())
        .chain(schema.cold_fields.iter())
        .find(|f| f.name == name)
        .ok_or_else(|| format!("field `{name}`: not declared in the schema"))
}

/// Hash-map (steel `hash?`) → ValueMap with kind coercion.
fn steel_hash_to_map(schema: &TableSchema, v: &SteelVal) -> Result<ValueMap, String> {
    let SteelVal::HashMapV(map) = v else {
        return Err(format!("values must be a hash (define/hash), got {v:?}"));
    };
    let mut out = BTreeMap::new();
    for (k, val) in map.iter() {
        let name = k
            .as_string()
            .ok_or_else(|| format!("hash keys must be strings, got {k:?}"))?
            .to_string();
        let f = find_field(schema, &name)?;
        out.insert(name.clone(), steel_to_value(&name, f.ty, &val)?);
    }
    Ok(out)
}

fn value_map_to_steel(m: &ValueMap) -> SteelVal {
    let mut inner = im_rc::HashMap::new();
    for (k, v) in m {
        inner.insert(StringV(k.as_str().to_owned().into()), value_to_steel(v));
    }
    SteelVal::HashMapV(SteelHashMap::from(steel::gc::Gc::new(inner)))
}

fn err(e: okm_dynamic::CodecError) -> Result<SteelVal, String> {
    Err(format!("{e}"))
}

/// Install the okm codec functions into a VM:
/// - `(okm-schema-from-json! "<json>")` → an opaque schema handle
/// - `(okm-schema-version! schema)` → layout version
/// - `(okm-encode-key! schema hash)` → bytevector
/// - `(okm-encode-payload! schema hash)` → bytevector
/// - `(okm-decode-key! schema bytes)` → hash
/// - `(okm-decode-payload! schema bytes)` → hash
thread_local! {
    static SCHEMAS: std::cell::RefCell<std::collections::HashMap<u64, std::sync::Arc<TableSchema>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}
fn next_schema_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    std::sync::atomic::AtomicU64::fetch_add(&NEXT, 1, std::sync::atomic::Ordering::Relaxed)
}
fn with_schema<T>(id: isize, f: impl FnOnce(&TableSchema) -> T) -> Result<T, String> {
    SCHEMAS.with(|r| {
        r.borrow()
            .get(&(id as u64))
            .map(|s| f(s))
            .ok_or_else(|| format!("okm: unknown schema handle {id}"))
    })
}
fn get_schema(id: isize) -> Result<std::sync::Arc<TableSchema>, String> {
    SCHEMAS.with(|r| {
        r.borrow()
            .get(&(id as u64))
            .cloned()
            .ok_or_else(|| format!("okm: unknown schema handle {id}"))
    })
}
fn steel_hash_to_map_for(id: isize, vals: SteelVal) -> Result<ValueMap, String> {
    let s = get_schema(id)?;
    steel_hash_to_map(&s, &vals)
}

pub fn register(vm: &mut Engine) {
    vm.register_fn("okm-schema-from-json!", |json: String| -> Result<SteelVal, String> {
        let schema: TableSchema = serde_json::from_str(&json)
            .map_err(|e| format!("bad schema json: {e}"))?;
        let id = next_schema_id();
        SCHEMAS.with(|r| r.borrow_mut().insert(id, Arc::new(schema)));
        Ok(IntV(id as isize))
    });
    vm.register_fn("okm-schema-version!", |id: isize| -> Result<isize, String> {
        with_schema(id, |s| s.layout_version as isize)
    });
    vm.register_fn("okm-encode-key!", |id: isize, vals: SteelVal| -> Result<SteelVal, String> {
        let vals = steel_hash_to_map_for(id, vals)?;
        let s = get_schema(id)?;
        encode_key(&s, &vals)
            .map_err(|e| e.to_string())
            .map(|b| SteelVal::VectorV(steel::rvals::SteelVector::from(
            steel::gc::Gc::new(b.into_iter().map(|x| IntV(x as isize)).collect::<im_rc::Vector<SteelVal>>()),
        )))
    });
    vm.register_fn("okm-encode-payload!", |id: isize, vals: SteelVal| -> Result<SteelVal, String> {
        let vals = steel_hash_to_map_for(id, vals)?;
        let s = get_schema(id)?;
        encode_payload(&s, &vals)
            .map_err(|e| e.to_string())
            .map(|b| SteelVal::VectorV(steel::rvals::SteelVector::from(
            steel::gc::Gc::new(b.into_iter().map(|x| IntV(x as isize)).collect::<im_rc::Vector<SteelVal>>()),
        )))
    });
    fn steel_bytes_to_vec(v: &SteelVal) -> Result<Vec<u8>, String> {
        let SteelVal::VectorV(vec) = v else {
            return Err("bytes must be a vector of integers 0-255".to_string());
        };
        let mut out = Vec::with_capacity(vec.len());
        for item in vec.iter() {
            let SteelVal::IntV(i) = item else {
                return Err("bytes must be a vector of integers 0-255".to_string());
            };
            out.push(u8::try_from(*i).map_err(|_| "bytes must be 0-255".to_string())?);
        }
        Ok(out)
    }
    vm.register_fn("okm-decode-key!", |id: isize, b: SteelVal| -> Result<SteelVal, String> {
        let s = get_schema(id)?;
        decode_key(&s, &steel_bytes_to_vec(&b)?)
            .map(|m| value_map_to_steel(&m))
            .or_else(err)
    });
    vm.register_fn("okm-decode-payload!", |id: isize, b: SteelVal| -> Result<SteelVal, String> {
        let s = get_schema(id)?;
        decode_payload(&s, &steel_bytes_to_vec(&b)?)
            .map(|m| value_map_to_steel(&m))
            .or_else(err)
    });
}
