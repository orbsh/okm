//! Dynamic-segment frame codec (ADR-0012). Slot 1 holds ONE entry per
//! obj — the value is a list of self-describing frames:
//!
//! ```text
//! ([field-id][value-type u8][len varint][bytes])* — len uses the
//! prefix-monotonic wire codec (P1), shared with `VarInt<T>`; small
//! frames cost 1-2 header bytes instead of a fixed 4-byte length.
//! ```
//!
//! `field-id` is the field NAME's dictionary number (slots 2/3); u8 ids
//! with `0xFF` escaping to `[0xFF][u16 id]` (255 names pay one byte per
//! frame; growth is append-only — no renumbering, ever). Unknown value
//! types on decode are normal input for a dynamic reader: the frame is
//! skipped by its length, not fatal — a newer writer may store types an
//! older reader cannot see. Frames are written in insertion order;
//! readers address fields by id, not position.

use std::collections::BTreeMap;
use crate::model::wrappers::obj_value::ObjValueType;
use crate::model::wrappers::wire::{put_len, take_len};

/// Dynamic-segment value: the run-time counterpart of a decoded frame.
/// Typed (fixed-width, schema-checked) fields never use this — declared
/// fields decode through the compile-time `FieldDesc` path.
#[derive(Clone, Debug, PartialEq)]
pub enum DynamicValue {
    UInt(u64),
    /// Signed integer (typed path: `i8`..`i64` fields, `Offset` lifts).
    Int(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    /// Nothing stored; presence itself is the information.
    Null,
    Array(Vec<DynamicValue>),
    /// Nested object: name-keyed map, the in-memory counterpart of a
    /// nested nTLV list (ADR-0012 — a map IS an nTLV list; the field
    /// NAME → id mapping reuses the same field-name dictionary).
    Obj(BTreeMap<String, DynamicValue>),
}

/// One dynamic field: dictionary id + decoded value.
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicField {
    pub id: u16,
    pub value: DynamicValue,
}

/// Append one frame. `id < 0xFF` takes the short form; anything larger
/// escapes through `0xFF`. Nested `DynamicValue::Obj` values are NOT
/// supported here (their ids must be resolved first) — use
/// [`put_frame_named`] with a name resolver.
pub fn put_frame(buf: &mut Vec<u8>, id: u16, value: &DynamicValue) {
    if id < 0xFF {
        buf.push(id as u8);
    } else {
        buf.push(0xFF);
        buf.extend_from_slice(&id.to_be_bytes());
    }
    // No dictionary here (the id is caller-resolved): a nested Obj value
    // cannot allocate names — the resolver panics instead of silently
    // emitting an empty body (the documented put_frame contract). Arrays
    // of scalars are fine.
    let mut no_dict = |name: &str| -> u16 {
        panic!(
            "put_frame: nested object requires the dictionary (name `{name}` unresolved) — use put_frame_named"
        );
    };
    let (ty, mut body) = encode_value(value, &mut no_dict);
    buf.push(ty.to_byte());
    put_len(buf, body.len());
    buf.append(&mut body);
}

/// Name resolver: returns the dictionary id for `name`, allocating
/// through the closure on first sight (the caller's dictionary owns
/// allocation discipline — single writer, engine mutex).
pub trait NameResolver {
    fn resolve(&mut self, name: &str) -> u16;
}

impl<F: FnMut(&str) -> u16> NameResolver for F {
    fn resolve(&mut self, name: &str) -> u16 {
        self(name)
    }
}

/// Append one frame for a NAMED field: nested `Obj` values recurse,
/// each nested name resolved through the same resolver (nested objects
/// share the obj's dictionary — one vocabulary per table).
pub fn put_frame_named<R: NameResolver>(
    buf: &mut Vec<u8>,
    name: &str,
    value: &DynamicValue,
    dict: &mut R,
) {
    put_frame_named_inner(buf, name, value, dict);
}

fn put_frame_named_inner<R: NameResolver>(
    buf: &mut Vec<u8>,
    name: &str,
    value: &DynamicValue,
    dict: &mut R,
) {
    let id = dict.resolve(name);
    if id < 0xFF {
        buf.push(id as u8);
    } else {
        buf.push(0xFF);
        buf.extend_from_slice(&id.to_be_bytes());
    }
    match value {
        DynamicValue::Obj(map) => {
            // Nested obj: one outer frame (type 7) whose body is the
            // nested nTLV list — same shape, same dictionary.
            let mut body = Vec::new();
            for (k, v) in map {
                put_frame_named_inner(&mut body, k, v, dict);
            }
            buf.push(ObjValueType::Obj.to_byte());
            put_len(buf, body.len());
            buf.append(&mut body);
        }
        other => {
            let (ty, mut body) = encode_value(other, dict);
            buf.push(ty.to_byte());
            put_len(buf, body.len());
            buf.append(&mut body);
        }
    }
}

/// Value → (type tag, body bytes). Nested objects need the dictionary
/// to resolve names → ids, so the resolver is a parameter (top-level
/// frames go through [`put_frame`], which owns the dict).
/// Value → (type tag, body bytes). Nested objects need the dictionary
/// to resolve names → ids, so the resolver is a parameter: a top-level
/// Obj is encoded by `put_frame_named`'s own arm, but an Obj nested
/// inside an Array reaches THIS arm — composites must encode fully
/// wherever they appear.
fn encode_value<R: NameResolver>(value: &DynamicValue, dict: &mut R) -> (ObjValueType, Vec<u8>) {
    match value {
        DynamicValue::Int(v) => {
            // Minimal big-endian two's-complement width.
            let be = v.to_be_bytes();
            let fill = if *v < 0 { 0xFF } else { 0x00 };
            let first = be.iter().position(|&b| b != fill).unwrap_or(7);
            // Keep one sign byte so the sign is recoverable.
            let start = if *v < 0 && first > 0 { first - 1 } else { first };
            (ObjValueType::Int, be[start..].to_vec())
        }
        DynamicValue::UInt(v) => {
            // Minimal big-endian width: strip leading zero bytes.
            let be = v.to_be_bytes();
            let first = be.iter().position(|&b| b != 0).unwrap_or(7);
            (ObjValueType::UInt, be[first..].to_vec())
        }
        DynamicValue::F64(v) => (ObjValueType::F64, v.to_be_bytes().to_vec()),
        DynamicValue::Str(s) => (ObjValueType::Str, s.as_bytes().to_vec()),
        DynamicValue::Bytes(b) => (ObjValueType::Bytes, b.clone()),
        DynamicValue::Bool(b) => (ObjValueType::Bool, vec![*b as u8]),
        DynamicValue::Null => (ObjValueType::Null, Vec::new()),
        DynamicValue::Array(items) => {
            // [element count u32 BE][element frames...]
            let mut body = Vec::new();
            put_len(&mut body, items.len());
            for item in items {
                let (ty, mut b) = encode_value(item, dict);
                body.push(ty.to_byte());
                put_len(&mut body, b.len());
                body.append(&mut b);
            }
            (ObjValueType::Array, body)
        }
        DynamicValue::Obj(map) => {
            // Obj inside an Array: the same nested-nTLV body the
            // frame-level Obj arm produces — same dictionary recursion.
            let mut body = Vec::new();
            for (k, v) in map {
                put_frame_named_inner(&mut body, k, v, dict);
            }
            (ObjValueType::Obj, body)
        }
    }
}

/// Decode one frame starting at `*off`; advances past it. `None` on
/// malformed input (truncated header/body) or an unknown type tag —
/// callers treat both as normal dynamic-reader conditions. An unknown
/// type is reported separately so the skip-by-length path stays
/// available to the list decoder.
#[derive(Debug)]
enum FrameErr {
    Malformed,
    UnknownType,
}

fn take<'a>(b: &'a [u8], off: &mut usize, n: usize) -> Option<&'a [u8]> {
    if *off + n > b.len() {
        return None;
    }
    let s = &b[*off..*off + n];
    *off += n;
    Some(s)
}

/// Decode the whole dynamic-segment value: frames until bytes run out.
/// Unknown value types are skipped by length (forward compatibility);
/// malformed frames abort the list — everything before the abort point
/// is still returned (partial results beat nothing for a dynamic
/// reader).
/// Decode all frames, resolving nested object field ids to names via
/// `name_of` (the dictionary's id → name direction). Top-level frames
/// keep their numeric ids in [`DynamicField`] (the Collection layer maps
/// them); nested `Obj` values come back fully name-keyed.
pub fn decode_named(
    bytes: &[u8],
    name_of: &mut dyn FnMut(u16) -> Option<String>,
) -> Vec<DynamicField> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < bytes.len() {
        match decode_frame_named(bytes, &mut off, name_of) {
            Ok(Some(f)) => out.push(f),
            Ok(None) => {}         // unknown type — skipped, off advanced
            Err(FrameErr::Malformed) => break, // torn tail: stop
            Err(FrameErr::UnknownType) => {}   // defensive
        }
    }
    out
}

fn decode_frame(bytes: &[u8], off: &mut usize) -> Result<Option<DynamicField>, FrameErr> {
    let mut no_names = |_id: u16| -> Option<String> { None };
    decode_frame_named(bytes, off, &mut no_names)
}

pub fn decode_variants(bytes: &[u8]) -> Vec<DynamicField> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < bytes.len() {
        match decode_frame(bytes, &mut off) {
            Ok(Some(f)) => out.push(f),
            // Unknown type: frame skipped (off already past its body);
            // the list continues with what follows.
            Err(FrameErr::UnknownType) | Ok(None) => {}
            // Malformed: stop; keep what we have.
            Err(FrameErr::Malformed) => break,
        }
    }
    out
}

fn decode_id(bytes: &[u8], off: &mut usize) -> Option<u16> {
    let first = *take(bytes, off, 1)?.first()?;
    if first == 0xFF {
        let b = take(bytes, off, 2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    } else {
        Some(first as u16)
    }
}

fn decode_frame_named(
    bytes: &[u8],
    off: &mut usize,
    name_of: &mut dyn FnMut(u16) -> Option<String>,
) -> Result<Option<DynamicField>, FrameErr> {
    let id = decode_id(bytes, off).ok_or(FrameErr::Malformed)?;
    let ty_byte = take(bytes, off, 1)
        .and_then(|b| b.first().copied())
        .ok_or(FrameErr::Malformed)?;
    let (len, len_n) = take_len(&bytes[*off..]).ok_or(FrameErr::Malformed)?;
    *off += len_n;
    let body = take(bytes, off, len).ok_or(FrameErr::Malformed)?;
    let ty = match ObjValueType::from_byte(ty_byte) {
        Some(t) => t,
        None => return Err(FrameErr::UnknownType), // caller may skip; off already advanced
    };
    let value = match ty {
        ObjValueType::Obj => {
            let mut m = BTreeMap::new();
            let mut inner = 0usize;
            while inner < body.len() {
                let nf = match decode_frame_named(body, &mut inner, name_of) {
                    Ok(Some(nf)) => Some(nf),
                    Ok(None) => None,               // unknown nested type
                    Err(_) => return Err(FrameErr::Malformed),
                };
                let Some(nf) = nf else { continue };
                let key = name_of(nf.id).unwrap_or_else(|| nf.id.to_string());
                m.insert(key, nf.value);
            }
            DynamicValue::Obj(m)
        }
        other => decode_value_named(other, body, name_of).ok_or(FrameErr::Malformed)?,
    };
    Ok(Some(DynamicField { id, value }))
}

/// Scalar/composite value decode. `name_of` resolves nested Obj frame
/// ids to names — an Obj inside an Array decodes through HERE, so the
/// resolver must be threaded down (composites are fully name-keyed
/// wherever they appear).
fn decode_value(ty: ObjValueType, body: &[u8]) -> Option<DynamicValue> {
    decode_value_named(ty, body, &mut |_id| None)
}

fn decode_value_named(ty: ObjValueType, body: &[u8], name_of: &mut dyn FnMut(u16) -> Option<String>) -> Option<DynamicValue> {
    Some(match ty {
        ObjValueType::UInt => {
            // Zero-padded back to 8 bytes, big-endian.
            if body.len() > 8 {
                return None;
            }
            let mut be = [0u8; 8];
            be[8 - body.len()..].copy_from_slice(body);
            DynamicValue::UInt(u64::from_be_bytes(be))
        }
        ObjValueType::Int => {
            // Sign-extend back to 8 bytes, big-endian two's complement.
            if body.is_empty() || body.len() > 8 {
                return None;
            }
            let fill = if body[0] & 0x80 != 0 { 0xFF } else { 0x00 };
            let mut be = [fill; 8];
            be[8 - body.len()..].copy_from_slice(body);
            DynamicValue::Int(i64::from_be_bytes(be))
        }
        ObjValueType::F64 => {
            if body.len() != 8 {
                return None;
            }
            DynamicValue::F64(f64::from_be_bytes([body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7]]))
        }
        ObjValueType::Str => DynamicValue::Str(std::str::from_utf8(body).ok()?.to_owned()),
        ObjValueType::Bytes => DynamicValue::Bytes(body.to_vec()),
        ObjValueType::Bool => {
            if body.len() != 1 {
                return None;
            }
            DynamicValue::Bool(body[0] != 0)
        }
        ObjValueType::Null => {
            if !body.is_empty() {
                return None;
            }
            DynamicValue::Null
        }
        ObjValueType::Array => {
            let (count, mut off) = take_len(body)?;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let ty_byte = *body.get(off)?;
                off += 1;
                let (len, ln) = take_len(body.get(off..)?)?;
                off += ln;
                let item_body = body.get(off..off + len)?;
                off += len;
                let ty = ObjValueType::from_byte(ty_byte)?;
                items.push(decode_value_named(ty, item_body, name_of)?);
            }
            DynamicValue::Array(items)
        }
        // Obj inside an Array: same nested-frame decode as
        // decode_frame_named's Obj arm (same resolver).
        ObjValueType::Obj => {
            let mut m = BTreeMap::new();
            let mut inner = 0usize;
            while inner < body.len() {
                let nf = match decode_frame_named(body, &mut inner, name_of) {
                    Ok(Some(nf)) => nf,
                    Ok(None) => continue,            // unknown nested type
                    Err(_) => return None,           // malformed
                };
                let key = name_of(nf.id).unwrap_or_else(|| nf.id.to_string());
                m.insert(key, nf.value);
            }
            DynamicValue::Obj(m)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obj_in_array_roundtrip() {
        // An Obj nested inside an Array: the value must encode with real
        // nested frames and decode fully name-keyed (composites wherever
        // they appear — this shape carried aura's persisted interface_schema).
        let mut nested = std::collections::BTreeMap::new();
        nested.insert("c".to_string(), DynamicValue::Int(0));
        let val = DynamicValue::Obj(std::collections::BTreeMap::from([(
            "item".to_string(),
            DynamicValue::Obj(nested),
        )]));
        let mut body = Vec::new();
        let mut resolver = |name: &str| match name {
            "a" => 1,
            "item" => 2,
            "c" => 3,
            _ => 99,
        };
        put_frame_named(&mut body, "a", &DynamicValue::Array(vec![val]), &mut resolver);
        let frames = decode_named(&body, &mut |id| match id {
            1 => Some("a".into()),
            2 => Some("item".into()),
            3 => Some("c".into()),
            _ => None,
        });
        assert_eq!(frames.len(), 1);
        let DynamicValue::Array(items) = &frames[0].value else {
            panic!("expected array, got {:?}", frames[0].value);
        };
        assert_eq!(items.len(), 1);
        let DynamicValue::Obj(m) = &items[0] else {
            panic!("nested obj inside array must decode, got {:?}", items[0]);
        };
        assert!(m.contains_key("item"), "nested obj keys resolve: {m:?}");
    }

    #[test]


    fn frames_round_trip() {
        let cases: Vec<(u16, DynamicValue)> = vec![
            (1, DynamicValue::UInt(0)),
            (2, DynamicValue::UInt(42)),
            (3, DynamicValue::UInt(u64::MAX)),
            (1, DynamicValue::F64(3.25)),
            (4, DynamicValue::Str("hello".into())),
            (5, DynamicValue::Str("你好".into())),
            (6, DynamicValue::Bytes(vec![0, 1, 0xFF])),
            (7, DynamicValue::Bool(true)),
            (8, DynamicValue::Bool(false)),
            (9, DynamicValue::Null),
            (
                10,
                DynamicValue::Array(vec![
                    DynamicValue::UInt(1),
                    DynamicValue::Str("x".into()),
                    DynamicValue::Null,
                ]),
            ),
        ];
        for (id, v) in &cases {
            let mut buf = Vec::new();
            put_frame(&mut buf, *id, v);
            let mut off = 0usize;
            let f = decode_frame(&buf, &mut off);
            if f.is_err() {
                panic!("case id={} v={:?} buf={:02x?} err={:?}", id, v, buf, f);
            }
            let f = f.unwrap().unwrap();
            assert_eq!(f.id, *id);
            assert_eq!(f.value, *v);
            assert_eq!(off, buf.len(), "frame consumed exactly");
        }
    }

    #[test]
    fn short_ids_take_one_byte_and_large_escape() {
        let mut buf = Vec::new();
        put_frame(&mut buf, 0xFE, &DynamicValue::UInt(1));
        assert_eq!(buf[0], 0xFE, "id 254 stays in the u8 lane");
        let mut buf = Vec::new();
        put_frame(&mut buf, 0xFF, &DynamicValue::UInt(1));
        assert_eq!(&buf[..3], &[0xFF, 0x00, 0xFF], "id 255 escapes");
        let mut buf = Vec::new();
        put_frame(&mut buf, 0x1234, &DynamicValue::UInt(1));
        assert_eq!(&buf[..3], &[0xFF, 0x12, 0x34]);
        let fields = decode_variants(&buf);
        assert_eq!(fields[0].id, 0x1234);
    }

    #[test]
    fn list_decode_is_partial_on_malformed_tail() {
        let mut buf = Vec::new();
        put_frame(&mut buf, 1, &DynamicValue::UInt(7));
        buf.extend_from_slice(&[0x05, 0x00, 0x09, 0xAA]); // torn tail: len 9, only 1 byte follows
        let fields = decode_variants(&buf);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].value, DynamicValue::UInt(7));
    }

    #[test]
    fn unknown_value_type_skips_by_length() {
        let mut buf = Vec::new();
        put_frame(&mut buf, 1, &DynamicValue::UInt(7));
        // Hand-rolled frame with a not-yet-defined type (0xFE).
        buf.extend_from_slice(&[0x02, 0xFE, 0x03, 0xAA, 0xBB, 0xCC]);
        put_frame(&mut buf, 3, &DynamicValue::Str("after".into()));
        let fields = decode_variants(&buf);
        assert_eq!(fields.len(), 2, "unknown frame skipped by length");
        assert_eq!(fields[0].id, 1);
        assert_eq!(fields[1].id, 3);
    }
}

#[cfg(test)]
mod nested_tests {
    use super::*;

    // Minimal dictionary stub for tests: name -> id by order of first sight.
    struct StubDict {
        next: u16,
        names: std::collections::HashMap<String, u16>,
    }
    impl StubDict {
        fn new() -> Self {
            StubDict { next: 0, names: std::collections::HashMap::new() }
        }
    }
    impl NameResolver for StubDict {
        fn resolve(&mut self, name: &str) -> u16 {
            if let Some(&id) = self.names.get(name) {
                return id;
            }
            let id = self.next;
            self.next += 1;
            self.names.insert(name.to_string(), id);
            id
        }
    }

    #[test]
    fn nested_obj_roundtrip() {
        let mut inner = BTreeMap::new();
        inner.insert("lat".to_string(), DynamicValue::F64(52.3));
        inner.insert("lon".to_string(), DynamicValue::F64(4.9));
        let mut outer = BTreeMap::new();
        outer.insert("loc".to_string(), DynamicValue::Obj(inner));
        outer.insert("n".to_string(), DynamicValue::UInt(7));

        let mut dict = StubDict::new();
        let mut buf = Vec::new();
        for (k, v) in &outer {
            put_frame_named(&mut buf, k, v, &mut dict);
        }

        // Decode with name resolution.
        let name_of_id = |id: u16| -> Option<String> {
            dict.names
                .iter()
                .find(|ent| *ent.1 == id)
                .map(|(k, _)| k.clone())
        };
        let fields = decode_named(&buf, &mut |id| name_of_id(id));
        assert_eq!(fields.len(), 2);

        let by_name: BTreeMap<&str, &DynamicValue> = fields
            .iter()
            .filter_map(|f| {
                let name = dict
                    .names
                    .iter()
                    .find(|ent| *ent.1 == f.id)
                    .map(|(k, _)| k.as_str())?;
                Some((name, &f.value))
            })
            .collect();
        assert_eq!(by_name["n"], &DynamicValue::UInt(7));
        match by_name["loc"] {
            DynamicValue::Obj(m) => {
                assert_eq!(m.len(), 2);
                assert_eq!(m["lat"], DynamicValue::F64(52.3));
                assert_eq!(m["lon"], DynamicValue::F64(4.9));
            }
            other => panic!("expected nested Obj, got {other:?}"),
        }
    }

    #[test]
    fn nested_deep_recursion() {
        // obj → obj → obj: three levels, all through the same dictionary.
        let leaf = BTreeMap::from([("v".to_string(), DynamicValue::Int(-3))]);
        let mid = BTreeMap::from([("leaf".to_string(), DynamicValue::Obj(leaf))]);
        let top = BTreeMap::from([("mid".to_string(), DynamicValue::Obj(mid))]);

        let mut dict = StubDict::new();
        let mut buf = Vec::new();
        for (k, v) in &top {
            put_frame_named(&mut buf, k, v, &mut dict);
        }
        // Dictionary saw: mid, leaf, v — shared vocabulary across levels.
        assert_eq!(dict.names.len(), 3);

        let name_of_id = |id: u16| -> Option<String> {
            dict.names
                .iter()
                .find(|ent| *ent.1 == id)
                .map(|(k, _)| k.clone())
        };
        let fields = decode_named(&buf, &mut |id| name_of_id(id));
        let DynamicValue::Obj(mid_m) = &fields[0].value else {
            panic!("top must be Obj");
        };
        let DynamicValue::Obj(leaf_m) = &mid_m["leaf"] else {
            panic!("mid must contain Obj leaf");
        };
        assert_eq!(leaf_m["v"], DynamicValue::Int(-3));
    }
}
