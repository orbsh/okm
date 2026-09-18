//! `Vector<T>` — a typed homogeneous list, used as a whole (ADR-0015 §4,
//! PLAN P3.5).
//!
//! Storage form: a variable-length frame — `[count u32 BE]` + `count ×`
//! element encoding (cold TLV frame in the payload's dynamic part, same
//!落位 as `String`). Element encoding by `T`:
//!
//! - fixed-width scalars (`f32`/`u32`/`i32`/`u64`/`i64`): bare little-
//!   endian values — zero per-element overhead (the homogeneous payoff
//!   versus `DynamicValue::Array`'s per-element frames);
//! - dynamic-width elements (`String`): per-element `[len u32][bytes]`
//!   (LV).
//!
//! No length in the type: the frame carries its own count, so a changed
//! embedding model (384 -> 768 dims) is just different data, not a wire
//! migration. Application-level count contracts are `#[ok_len(N)]` on
//! the declaring field / `FieldSchema::expect_len` in the dynamic reader
//! — decode-time checks, not format constraints.
//!
//! Multi-dimensional shape is an application-layer interpretation of the
//! flat element sequence (row-major by convention); nothing on the wire.
//!
//! Elements are pure values — the identity dividing line (ADR-0015 §3)
//! keeps this out of Ref/Junction: a scalar has no key, a key reference
//! to it is a category error.

/// A typed variable-length homogeneous list. `T` is the element type;
/// memory form is `Vec<T>`, wire form is the frame described above.
#[derive(Clone, Debug, PartialEq)]
pub struct Vector<T> {
    pub elems: Vec<T>,
}

impl<T> Vector<T> {
    pub fn new(elems: Vec<T>) -> Self {
        Vector { elems }
    }

    pub fn len(&self) -> usize {
        self.elems.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elems.is_empty()
    }
}

impl<T: Default> Default for Vector<T> {
    fn default() -> Self {
        Vector { elems: Vec::new() }
    }
}

/// Element codec: fixed-width scalars only (little-endian, bare V in the
/// frame). `String` elements are handled inline by the derive (per-element
/// LV) — they have no `VectorElem` impl, so a `Vector<String>` cannot be
/// hand-encoded through this trait (the derive path is the only writer,
/// keeping one codec per shape).
pub trait VectorElem: Copy {
    const ELEM_WIDTH: usize;
    /// Append the little-endian encoding of one element.
    fn encode_le(&self, buf: &mut Vec<u8>);
    /// Decode one element from a little-endian slice.
    fn decode_le(b: &[u8]) -> Self;
}

impl VectorElem for f32 {
    const ELEM_WIDTH: usize = 4;
    fn encode_le(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
    fn decode_le(b: &[u8]) -> Self { f32::from_le_bytes(b.try_into().unwrap()) }
}
impl VectorElem for u32 {
    const ELEM_WIDTH: usize = 4;
    fn encode_le(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
    fn decode_le(b: &[u8]) -> Self { u32::from_le_bytes(b.try_into().unwrap()) }
}
impl VectorElem for i32 {
    const ELEM_WIDTH: usize = 4;
    fn encode_le(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
    fn decode_le(b: &[u8]) -> Self { i32::from_le_bytes(b.try_into().unwrap()) }
}
impl VectorElem for u64 {
    const ELEM_WIDTH: usize = 8;
    fn encode_le(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
    fn decode_le(b: &[u8]) -> Self { u64::from_le_bytes(b.try_into().unwrap()) }
}
impl VectorElem for i64 {
    const ELEM_WIDTH: usize = 8;
    fn encode_le(&self, buf: &mut Vec<u8>) { buf.extend_from_slice(&self.to_le_bytes()); }
    fn decode_le(b: &[u8]) -> Self { i64::from_le_bytes(b.try_into().unwrap()) }
}

impl<T: VectorElem> Vector<T> {
    /// Encode the frame payload (count prefix + bare elements) — the cold
    /// TLV loop wraps it in the `[tag][len]` frame header.
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 + self.elems.len() * T::ELEM_WIDTH);
        buf.extend_from_slice(&(self.elems.len() as u32).to_be_bytes());
        for e in &self.elems {
            e.encode_le(&mut buf);
        }
        buf
    }

    /// Decode a frame payload (count prefix + elements). The wire has no
    /// length expectation — whatever count the frame carries is read.
    pub fn decode_payload(payload: &[u8]) -> Self {
        let count = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
        let mut elems = Vec::with_capacity(count);
        let mut p = 4usize;
        for _ in 0..count {
            elems.push(T::decode_le(&payload[p..p + T::ELEM_WIDTH]));
            p += T::ELEM_WIDTH;
        }
        Vector { elems }
    }
}
