//! nTLV: `[tag varint][kind u8][payload]`, repeated until the record's length runs
//! out.
//!
//! The point of typing the payload is that a reader can look without a schema: a
//! caller that knows the tag gets the value, and a caller that does not can still
//! skip it exactly. Strings are the interesting kind — they are the ones a
//! dictionary will eventually replace with a code, which is why their kind byte is
//! the one that grows a meaning (kind `16 + k` = "the id of a string in dictionary
//! block k").

use crate::dict::Dict;
use crate::format::{get_svarint, get_varint, put_svarint, put_varint};

pub const KIND_BYTES: u8 = 0;
pub const KIND_STR: u8 = 1;
pub const KIND_U64: u8 = 2;
pub const KIND_I64: u8 = 3;
pub const KIND_F64: u8 = 4;

/// The first kind code that means "a code in dictionary block `kind - DICT_BASE`".
pub const DICT_BASE: u8 = 16;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bytes(Vec<u8>),
    Str(String),
    U64(u64),
    I64(i64),
    F64(f64),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::U64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::F64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b.as_slice()),
            _ => None,
        }
    }
}

/// A record body: tag-ordered, no nulls (an absent tag is an absent field).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Body {
    items: Vec<(u16, Value)>,
}

impl Body {
    pub fn new() -> Self {
        Body { items: Vec::new() }
    }

    pub fn push(&mut self, tag: u16, value: Value) -> &mut Self {
        self.items.push((tag, value));
        self
    }

    pub fn get(&self, tag: u16) -> Option<&Value> {
        self.items.iter().find(|(t, _)| *t == tag).map(|(_, v)| v)
    }

    pub fn items(&self) -> &[(u16, Value)] {
        self.items.as_slice()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn encode(&self) -> Vec<u8> {
        self.encode_with(None)
    }

    /// Encode with a dictionary in hand: a string the dictionary has settled becomes a
    /// code (`kind = DICT_BASE + block`, then that block's width in bytes), and
    /// everything else stays inline.
    pub fn encode_with(&self, dict: Option<&mut Dict>) -> Vec<u8> {
        let mut dict = dict;
        let mut out = Vec::with_capacity(16 * self.items.len());
        for (tag, value) in &self.items {
            put_varint(&mut out, u64::from(*tag));
            match value {
                Value::Bytes(bytes) => {
                    out.push(KIND_BYTES);
                    put_varint(&mut out, bytes.len() as u64);
                    out.extend_from_slice(bytes);
                }
                Value::Str(text) => {
                    let settled = match dict.as_deref_mut() {
                        Some(dict) => dict.settle(text),
                        None => None,
                    };
                    match settled {
                        Some((code, id)) => {
                            let width =
                                dict.as_ref().and_then(|dict| dict.width(code)).unwrap_or(4);
                            out.push(code);
                            let bytes = id.to_be_bytes();
                            out.extend_from_slice(&bytes[8 - width..]);
                        }
                        None => {
                            out.push(KIND_STR);
                            put_varint(&mut out, text.len() as u64);
                            out.extend_from_slice(text.as_bytes());
                        }
                    }
                }
                Value::U64(v) => {
                    out.push(KIND_U64);
                    put_varint(&mut out, *v);
                }
                Value::I64(v) => {
                    out.push(KIND_I64);
                    put_svarint(&mut out, *v);
                }
                Value::F64(v) => {
                    out.push(KIND_F64);
                    out.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        out
    }

    /// Decode a body. Dictionary codes are not resolved here yet: a `kind` of
    /// `DICT_BASE` or above comes back as an error rather than as a silently wrong
    /// string (see ADR-0046 §5 — the dictionary is the next slice).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        Self::decode_with(bytes, None)
    }

    /// Decode with the part's dictionary in hand. A code whose block is missing is an
    /// error, never a silently wrong string.
    pub fn decode_with(bytes: &[u8], dict: Option<&Dict>) -> Option<Self> {
        let mut items = Vec::new();
        let mut pos = 0usize;
        while pos < bytes.len() {
            let tag = u16::try_from(get_varint(bytes, &mut pos)?).ok()?;
            let kind = *bytes.get(pos)?;
            pos += 1;
            let value = match kind {
                KIND_BYTES => {
                    let len = usize::try_from(get_varint(bytes, &mut pos)?).ok()?;
                    let end = pos.checked_add(len)?;
                    let bytes = bytes.get(pos..end)?.to_vec();
                    pos = end;
                    Value::Bytes(bytes)
                }
                KIND_STR => {
                    let len = usize::try_from(get_varint(bytes, &mut pos)?).ok()?;
                    let end = pos.checked_add(len)?;
                    let text = String::from_utf8(bytes.get(pos..end)?.to_vec()).ok()?;
                    pos = end;
                    Value::Str(text)
                }
                KIND_U64 => Value::U64(get_varint(bytes, &mut pos)?),
                KIND_I64 => Value::I64(get_svarint(bytes, &mut pos)?),
                code if code >= DICT_BASE => {
                    let dict = dict?;
                    let width = dict.width(code)?;
                    let raw = bytes.get(pos..pos + width)?;
                    pos += width;
                    let mut id = 0u64;
                    for (index, byte) in raw.iter().enumerate() {
                        id |= u64::from(*byte) << (8 * (width - 1 - index));
                    }
                    Value::Str(dict.lookup(code, id)?.to_string())
                }
                KIND_F64 => {
                    let raw = bytes.get(pos..pos + 8)?;
                    pos += 8;
                    Value::F64(f64::from_be_bytes(raw.try_into().ok()?))
                }
                _ => return None,
            };
            items.push((tag, value));
        }
        Some(Body { items })
    }
}
