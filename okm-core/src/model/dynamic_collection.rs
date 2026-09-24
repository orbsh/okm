//! DynamicCollection — runtime-ns keyed documents (ADR-0025): a
//! collection whose namespace is a run-time value and whose keys are
//! application-declared field lists. The declared `Collection` stays the
//! default; this is the escape hatch for host-assembled schemas (aura
//! ADR-0026: one real ns per actor type, allocated from a registry).
//!
//! Key wire: ORDER-PRESERVING. Per field `[id u16 BE][body]`:
//! UInt/Bool fixed-width BE, Int sign-bit flipped BE, F64 total-order
//! remap BE, Str/Bytes 0x00-escaped terminator, Null empty. Byte order
//! equals value order so `scan_prefix`/`scan_range` semantics are honest.
//! Document VALUES reuse the nTLV dynamic-segment frames + dictionary
//! (ADR-0012) unchanged.

use crate::engine::storage::VirtualStorage;
use crate::model::index::PRIMARY_SLOT;
use crate::model::obj_dict::DictCache;
use crate::model::obj_dynamic::{decode_named, put_frame_named};
use crate::obj_dynamic::DynamicValue;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One key field: name + value (order defines the encoding).
pub type KeyField = (String, DynamicValue);
/// A document: name-keyed dynamic fields (the value side).
pub type Doc = BTreeMap<String, DynamicValue>;

pub struct DynamicCollection<S> {
    store: S,
    /// The runtime namespace: `[ns_hi, ns_lo]` big-endian — the same
    /// position a declared table's `NS_PREFIX` occupies.
    ns: [u8; 2],
    /// Field-name dictionary (key field names + document field names
    /// share one vocabulary per collection, as declared tables do).
    dict: Mutex<DictCache>,
}

fn key_err(v: &DynamicValue) -> String {
    format!("unsupported key field value: {v:?} (Array/Obj keys are rejected — flatten first)")
}

/// Order-preserving body for one key value (ADR-0025 §2).
fn key_body(value: &DynamicValue) -> Result<Vec<u8>, String> {
    Ok(match value {
        DynamicValue::Null => Vec::new(),
        DynamicValue::Bool(b) => vec![*b as u8],
        DynamicValue::UInt(v) => v.to_be_bytes().to_vec(),
        DynamicValue::Int(v) => {
            // Flip the sign bit: two's complement BE compares as unsigned
            // after the top-bit remap (order-preserving transform).
            let mut be = v.to_be_bytes();
            be[0] ^= 0x80;
            be.to_vec()
        }
        DynamicValue::F64(v) => {
            // IEEE BE + sign-magnitude → magnitude-sign remap (standard
            // total order for floats; -0.0 < 0.0 resolves consistently).
            let bits = v.to_bits();
            let mapped = if bits >> 63 == 1 { !bits } else { bits ^ 0x8000_0000_0000_0000 };
            mapped.to_be_bytes().to_vec()
        }
        DynamicValue::Str(s) => escape_terminator(s.as_bytes()),
        DynamicValue::Bytes(b) => escape_terminator(b),
        other => return Err(key_err(other)),
    })
}

/// 0x00-escape terminator encoding: raw bytes, each 0x00 becomes
/// 0x00 0xFF, closed by a single 0x00. Lexicographic byte order is
/// preserved (the standard technique); embedded 0x00 cannot terminate.
fn escape_terminator(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    for &b in bytes {
        if b == 0x00 {
            out.extend_from_slice(&[0x00, 0xFF]);
        } else {
            out.push(b);
        }
    }
    out.push(0x00);
    out
}

/// Inverse of [`escape_terminator`]. Returns None on malformed input.
fn unescape_terminator(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x00 if i + 1 < bytes.len() && bytes[i + 1] == 0xFF => {
                out.push(0x00);
                i += 2;
            }
            0x00 => return Some(out), // terminator
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None // no terminator: malformed
}

/// Decode one order-preserving key field body back to a DynamicValue.
/// `id`'s name is resolved by the caller; the TYPE is recoverable from
/// the body only for fixed-width kinds — the caller carries the declared
/// kinds (see `KeySchema`) so decode is total.
fn key_body_decode(kind: KeyKind, body: &[u8]) -> Result<DynamicValue, String> {
    Ok(match kind {
        KeyKind::Null => DynamicValue::Null,
        KeyKind::Bool => DynamicValue::Bool(body.first().map(|&b| b != 0).unwrap_or(false)),
        KeyKind::UInt => {
            let mut be = [0u8; 8];
            if body.len() > 8 {
                return Err("uint key body overflows u64".into());
            }
            be[8 - body.len()..].copy_from_slice(body);
            DynamicValue::UInt(u64::from_be_bytes(be))
        }
        KeyKind::Int => {
            let mut be = [0u8; 8];
            if body.len() > 8 {
                return Err("int key body overflows i64".into());
            }
            be[8 - body.len()..].copy_from_slice(body);
            be[0] ^= 0x80; // undo the sign-bit remap
            DynamicValue::Int(i64::from_be_bytes(be))
        }
        KeyKind::F64 => {
            if body.len() != 8 {
                return Err("f64 key body must be 8 bytes".into());
            }
            let mut bits = u64::from_be_bytes(body.try_into().unwrap());
            bits = if bits >> 63 == 1 { bits & 0x7FFF_FFFF_FFFF_FFFF } else { !bits };
            DynamicValue::F64(f64::from_bits(bits))
        }
        KeyKind::Str => {
            let raw = unescape_terminator(body).ok_or("str key body missing terminator")?;
            DynamicValue::Str(String::from_utf8(raw).map_err(|e| e.to_string())?)
        }
        KeyKind::Bytes => {
            let raw = unescape_terminator(body).ok_or("bytes key body missing terminator")?;
            DynamicValue::Bytes(raw)
        }
    })
}

/// The key field kinds, in declaration order — carried by the caller
/// (the schema) so encoded bodies decode totally.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyKind { Null, Bool, UInt, Int, F64, Str, Bytes }

impl<S: VirtualStorage> DynamicCollection<S> {
    pub fn new(store: S, ns: u16) -> Self {
        Self { store, ns: ns.to_be_bytes(), dict: Mutex::new(DictCache::default()) }
    }

    pub fn ns_prefix(&self) -> [u8; 2] {
        self.ns
    }

    fn header(&self) -> Vec<u8> {
        self.ns.to_vec()
    }

    fn entry_key(&mut self, slot: u16, key: &[(String, DynamicValue)]) -> Result<Vec<u8>, String> {
        let mut buf = self.header();
        buf.extend_from_slice(&slot.to_be_bytes());
        self.encode_key_into(&mut buf, key)?;
        Ok(buf)
    }

    /// The key frame: `[id u16 BE][body]` per field, order-preserving.
    fn encode_key_into(&mut self, buf: &mut Vec<u8>, key: &[(String, DynamicValue)]) -> Result<(), String> {
        let mut d = self.dict.lock().unwrap();
        let header = self.header();
        for (name, value) in key {
            let id = d.id_for(&mut self.store, &header, name);
            buf.extend_from_slice(&id.to_be_bytes());
            buf.extend_from_slice(&key_body(value)?);
        }
        Ok(())
    }

    pub fn put(&mut self, key: &[(String, DynamicValue)], doc: &Doc) -> Result<(), String> {
        let pkey = self.entry_key(PRIMARY_SLOT, key)?;
        // Value: nTLV named frames through the dictionary (the dynamic
        // segment's exact wire — one vocabulary per collection).
        let mut body = Vec::new();
        let header = self.header();
        {
            let mut d = self.dict.lock().unwrap();
            let mut resolver = |name: &str| d.id_for(&mut self.store, &header, name);
            for (name, value) in doc {
                put_frame_named(&mut body, name, value, &mut resolver);
            }
        }
        if body.is_empty() {
            self.store.del(&pkey);
        } else {
            self.store.put(pkey, body);
        }
        Ok(())
    }

    /// Decode a primary entry's frames into a name-keyed document.
    fn decode_doc(&self, raw: &[u8]) -> Doc {
        let mut d = DictCache::default();
        let header = self.header();
        let frames = decode_named(raw, &mut |id| d.name_for(&self.store, &header, id));
        let mut out = BTreeMap::new();
        for f in frames {
            if let Some(name) = d.name_for(&self.store, &header, f.id) {
                out.insert(name, f.value);
            }
        }
        out
    }

    pub fn get(&mut self, key: &[(String, DynamicValue)]) -> Result<Option<Doc>, String> {
        let pkey = self.entry_key(PRIMARY_SLOT, key)?;
        Ok(self.store.get(&pkey).map(|raw| self.decode_doc(&raw)))
    }

    pub fn delete(&mut self, key: &[(String, DynamicValue)]) -> Result<(), String> {
        let pkey = self.entry_key(PRIMARY_SLOT, key)?;
        self.store.del(&pkey);
        Ok(())
    }

    /// Ordered prefix scan over the first `prefix.len()` key fields.
    /// Returns (key fields, document) pairs; `limit` caps the read.
    pub fn scan_prefix(
        &mut self,
        kinds: &[KeyKind],
        prefix: &[(String, DynamicValue)],
        limit: Option<u64>,
    ) -> Result<Vec<(Vec<DynamicValue>, Doc)>, String> {
        let begin = self.entry_key(PRIMARY_SLOT, prefix)?;
        let end = prefix_end_of(&begin);
        self.scan_range_inner(kinds, &begin, end.as_deref(), limit)
    }

    /// Ordered range scan over full key frames: `[begin, end)`.
    pub fn scan_range(
        &mut self,
        kinds: &[KeyKind],
        begin: Vec<u8>,
        end: Option<Vec<u8>>,
        limit: Option<u64>,
    ) -> Result<Vec<(Vec<DynamicValue>, Doc)>, String> {
        self.scan_range_inner(kinds, &begin, end.as_deref(), limit)
    }

    fn scan_range_inner(
        &mut self,
        kinds: &[KeyKind],
        begin: &[u8],
        end: Option<&[u8]>,
        limit: Option<u64>,
    ) -> Result<Vec<(Vec<DynamicValue>, Doc)>, String> {
        let mut out = Vec::new();
        for (full, raw) in self.store.scan_range_iter(begin, end) {
            if limit.map(|n| out.len() as u64 >= n).unwrap_or(false) {
                break;
            }
            // Strip [ns 2B][slot 2B] — the remainder is the key frame.
            let frame = &full[4..];
            let fields = self.decode_key_frame(kinds, frame)?;
            let doc = self.decode_doc(&raw);
            out.push((fields, doc));
        }
        Ok(out)
    }

    /// Test seam: key-body validation without touching the store.
    #[cfg(test)]
    fn encode_key_into_probe(&mut self, key: &[(String, DynamicValue)]) -> Result<(), String> {
        let mut buf = Vec::new();
        self.encode_key_into(&mut buf, key)
    }

    /// Decode a key frame back to values. Field kinds come from the
    /// declared schema (caller-carried); the frame itself carries only
    /// ids + order-preserving bodies.
    fn decode_key_frame(&self, kinds: &[KeyKind], frame: &[u8]) -> Result<Vec<DynamicValue>, String> {
        let mut d = DictCache::default();
        let header = self.header();
        let mut out = Vec::with_capacity(kinds.len());
        let mut i = 0;
        for kind in kinds {
            if i + 2 > frame.len() {
                return Err("key frame truncated".into());
            }
            let _id = u16::from_be_bytes([frame[i], frame[i + 1]]);
            i += 2;
            let body_end = key_body_end(kind, &frame[i..]);
            out.push(key_body_decode(*kind, &frame[i..i + body_end])?);
            i += body_end;
        }
        Ok(out)
    }
}

/// The byte length of one key field body, given its kind — Str/Bytes
/// need the terminator scan; fixed widths are known.
fn key_body_end(kind: &KeyKind, rest: &[u8]) -> usize {
    match kind {
        KeyKind::Null => 0,
        KeyKind::Bool => 1,
        KeyKind::UInt | KeyKind::Int => rest.len().min(8),
        KeyKind::F64 => 8,
        KeyKind::Str | KeyKind::Bytes => {
            // Scan for the unescaped terminator.
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    0x00 if i + 1 < rest.len() && rest[i + 1] == 0xFF => i += 2,
                    0x00 => return i + 1,
                    _ => i += 1,
                }
            }
            rest.len()
        }
    }
}

/// Exclusive upper bound of a full key: increment the last byte with
/// carry (the engine's own `prefix_end` discipline, applied to dynamic
/// frames). None when unbounded (all 0xFF).
fn prefix_end_of(begin: &[u8]) -> Option<Vec<u8>> {
    let mut end = begin.to_vec();
    for b in end.iter_mut().rev() {
        if *b < 0xFF {
            *b += 1;
            return Some(end);
        }
        *b = 0x00;
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_engine::TestStore;

    fn coll(ns: u16) -> DynamicCollection<TestStore> {
        let store = TestStore::matrix().remove(0).1;
        DynamicCollection::new(store, ns)
    }

    fn s(v: &str) -> DynamicValue { DynamicValue::Str(v.into()) }

    #[test]
    fn put_get_delete_roundtrip() {
        let mut c = coll(700);
        let key = vec![("user".to_string(), s("alice")), ("seq".to_string(), DynamicValue::UInt(3))];
        c.put(&key, &BTreeMap::from([
            ("items".to_string(), DynamicValue::UInt(42)),
            ("note".to_string(), s("hello")),
        ])).unwrap();
        let doc = c.get(&key).unwrap().unwrap();
        assert_eq!(doc["items"], DynamicValue::UInt(42));
        assert_eq!(doc["note"], s("hello"));
        c.delete(&key).unwrap();
        assert!(c.get(&key).unwrap().is_none());
    }

    #[test]
    fn key_order_is_value_order() {
        // Integers: -1 must sort before 0, before 1.
        let mut c = coll(701);
        let kinds = [KeyKind::Int];
        for v in [5i64, -1, 0, 100, -50] {
            c.put(&[("v".into(), DynamicValue::Int(v))], &BTreeMap::from([
                ("v".to_string(), DynamicValue::Int(v)),
            ])).unwrap();
        }
        let rows = c.scan_prefix(&kinds, &[], None).unwrap();
        let vals: Vec<i64> = rows.iter().map(|(_, d)| match d["v"] { DynamicValue::Int(v) => v, _ => panic!() }).collect();
        assert_eq!(vals, vec![-50, -1, 0, 5, 100], "int key order must be numeric order");
    }

    #[test]
    fn str_keys_order_and_embedded_null() {
        let mut c = coll(702);
        let kinds = [KeyKind::Str];
        for name in ["b", "a\u{0}b", "ab", "a"] {  // embedded NUL must not terminate early
            c.put(&[("name".into(), s(name))], &BTreeMap::from([
                ("name".to_string(), s(name)),
            ])).unwrap();
        }
        let rows = c.scan_prefix(&kinds, &[], None).unwrap();
        let names: Vec<String> = rows.iter().map(|(_, d)| match &d["name"] { DynamicValue::Str(s) => s.clone(), _ => panic!() }).collect();
        assert_eq!(names, vec!["a", "a\u{0}b", "ab", "b"], "escaped terminator keeps lexicographic order");
    }

    #[test]
    fn f64_total_order() {
        let mut c = coll(703);
        let kinds = [KeyKind::F64];
        for v in [1.5f64, -2.0, 0.0, -0.0, 100.25] {
            c.put(&[("v".into(), DynamicValue::F64(v))], &BTreeMap::from([
                ("v".to_string(), DynamicValue::F64(v)),
            ])).unwrap();
        }
        let rows = c.scan_prefix(&kinds, &[], None).unwrap();
        let vals: Vec<f64> = rows.iter().map(|(_, d)| match d["v"] { DynamicValue::F64(v) => v, _ => panic!() }).collect();
        assert_eq!(vals, vec![-2.0, 0.0, 0.0, 1.5, 100.25]); // -0.0 normalizes to 0.0 in the remap
    }

    #[test]
    fn scan_prefix_by_leading_field() {
        let mut c = coll(704);
        for (user, seq) in [("alice", 1), ("alice", 2), ("bob", 1)] {
            c.put(&[
                ("user".into(), s(user)),
                ("seq".into(), DynamicValue::UInt(seq)),
            ], &BTreeMap::from([
                ("seq".to_string(), DynamicValue::UInt(seq)),
            ])).unwrap();
        }
        let kinds = [KeyKind::Str, KeyKind::UInt];
        let rows = c.scan_prefix(&kinds, &[("user".into(), s("alice"))], None).unwrap();
        assert_eq!(rows.len(), 2, "prefix scan sees only alice's rows");
        // Distinct ns = disjoint keyspace (structural isolation).
        let mut other = coll(705);
        assert!(other.get(&[("user".into(), s("alice")), ("seq".into(), DynamicValue::UInt(1))]).unwrap().is_none());
    }

    #[test]
    fn obj_key_field_is_rejected() {
        let mut c = coll(706);
        assert!(c.encode_key_into_probe(&[("bad".into(), DynamicValue::Obj(Default::default()))]).is_err());
    }
}
