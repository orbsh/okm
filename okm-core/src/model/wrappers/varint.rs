//! `VarInt<T>` — variable-length unsigned integer with **byte order =
//! value order** (P1, UTF-8-style prefix-monotonic encoding).
//!
//! LEB128 (the previous encoding) compared bytes in numeric-value order
//! — small values' first byte was large — so a VarInt could not ride in
//! an index segment without a swap transform. This encoding fixes the
//! prefix in the FIRST byte: the number of leading 1-bits is the width,
//! the terminator is a 0 bit, remaining bytes are big-endian payload.
//!
//! ```text
//! width  first byte   payload bits   value range
//! 1      0xxxxxxx     7              0 .. 2^7-1
//! 2      10xxxxxx     6+8 = 14       2^7 .. 2^14-1
//! 3      110xxxxx     5+16 = 21      2^14 .. 2^21-1
//! 4      1110xxxx     4+24 = 28
//! 5      11110xxx     3+32 = 35
//! 6      111110xx     2+40 = 42
//! 7      1111110x     1+48 = 49
//! 8      11111110     0+56 = 56
//! 9      11111111     64             2^56 .. 2^64-1
//! ```
//!
//! Prefix bytes are strictly increasing across widths and widths are
//! monotone in value, so **lexicographic byte comparison equals numeric
//! comparison** — a VarInt is a legal index-segment field with no swap
//! transform. Payload is big-endian within a width. 9 bytes for
//! `u64::MAX` (vs LEB128's 10).
//!
//! The width/prefix arithmetic lives in [`super::wire`] — one
//! implementation shared with the frame-length prefix codec. Unsigned
//! only (`u16`/`u32`/`u64`; `u8` is already minimal-width). The frame's
//! own length absorbs width variability on the cold segment;
//! index-segment use rides the fixed per-width layout.

use super::wire;

/// Prefix-monotonic varint codec (see the module docs for the layout).
pub trait VarIntEnc: Copy + Sized {
    /// Maximum encoded width for this type (9 for u64, less for smaller).
    const MAX_WIDTH: usize;
    /// Write the encoding to `buf`.
    fn varint_encode(self, buf: &mut Vec<u8>);
    /// Decode from the front of `b`; returns the value and bytes consumed.
    /// Panics on truncated input or an overlong width for the type.
    fn varint_decode(b: &[u8]) -> (Self, usize);
    /// Bridge cast: u64 (DynamicValue::UInt) -> the concrete width.
    fn varint_from_u64(v: u64) -> Self;
}

macro_rules! impl_varint {
    ($t:ty) => {
        impl VarIntEnc for $t {
            const MAX_WIDTH: usize = 9;
            fn varint_from_u64(v: u64) -> Self {
                v as $t
            }
            fn varint_encode(self, buf: &mut Vec<u8>) {
                let v = self as u64;
                let w = wire::width_of(v);
                let mut out = vec![0u8; w];
                if w == 9 {
                    // 0xFF prefix + full 64-bit BE payload
                    out[0] = 0xFF;
                    out[1..].copy_from_slice(&v.to_be_bytes());
                } else {
                    let payload_bits = (w - 1) * 8 + (8 - w);
                    out[0] = wire::prefix(w) | ((v >> ((w - 1) * 8)) as u8 & (0xFF >> w));
                    let low = v & ((1u64 << ((w - 1) * 8)) - 1);
                    let lb = low.to_be_bytes();
                    out[1..].copy_from_slice(&lb[8 - (w - 1)..]);
                }
                buf.extend_from_slice(&out);
            }
            fn varint_decode(b: &[u8]) -> (Self, usize) {
                let w = wire::prefix_width(*b.first().expect("VarInt: truncated frame"));
                assert!(b.len() >= w, "VarInt: truncated frame");
                let v: u64 = if w == 9 {
                    u64::from_be_bytes(b[1..9].try_into().unwrap())
                } else {
                    let payload_bits = (w - 1) * 8 + (8 - w);
                    let hi = (b[0] & (0xFF >> w)) as u64;
                    let mut low: u64 = 0;
                    for i in 1..w {
                        low = (low << 8) | b[i] as u64;
                    }
                    (hi << (payload_bits - (8 - w))) | low
                };
                (v as $t, w)
            }
        }
    };
}
impl_varint!(u16);
impl_varint!(u32);
impl_varint!(u64);

/// Newtype the derive macros recognize in field position (`VarInt<u32>`).
/// Field declarations stay plain `T`; the wrapper is applied at encode
/// time and removed at decode time, so user code keeps working with raw
/// values. Wire is variable length: `FieldDesc::width` is 0 and the TLV
/// frame's `len` is authoritative — same regime as `String`. Byte order
/// = value order: legal in an index segment without transforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VarInt<T: VarIntEnc>(pub T);

impl<T: VarIntEnc> VarInt<T> {
    /// Bridge constructor (ADR-0012 document-map bridge): `T` inferred from
    /// the field type — no type interpolation in generated code.
    pub fn from_dyn(v: u64) -> Self {
        VarInt(T::varint_from_u64(v))
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(T::MAX_WIDTH);
        self.0.varint_encode(&mut buf);
        buf
    }
    /// Inverse of [`VarInt::encode`]; `b` starts with the encoding.
    pub fn decode(b: &[u8]) -> Self {
        VarInt(T::varint_decode(b).0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_shapes() {
        assert_eq!(VarInt::<u32>(0).encode(), vec![0x00]);
        assert_eq!(VarInt::<u32>(127).encode(), vec![0x7F]);
        assert_eq!(VarInt::<u32>(128).encode(), vec![0x80, 0x80]);
        assert_eq!(VarInt::<u32>(300).encode().len(), 2);
        assert_eq!(VarInt::<u64>(u64::MAX).encode().len(), 9);
    }

    #[test]
    fn round_trip_domain_edges() {
        for v in [0u64, 1, 127, 128, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            assert_eq!(VarInt::<u64>::decode(&VarInt(v).encode()).0, v);
        }
        for v in [0u16, 127, 128, u16::MAX] {
            assert_eq!(VarInt::<u16>::decode(&VarInt(v).encode()).0, v);
        }
        for v in [0u32, 127, 128, u32::MAX] {
            assert_eq!(VarInt::<u32>::decode(&VarInt(v).encode()).0, v);
        }
    }

    /// THE acceptance test: lexicographic byte order equals numeric order
    /// across width boundaries.
    #[test]
    fn byte_order_equals_value_order() {
        let values: Vec<u64> = vec![
            0, 1, 42, 126, 127, 128, 129, 2_097_151, 2_097_152, 268_435_455,
            268_435_456, u32::MAX as u64, u32::MAX as u64 + 1, u64::MAX - 1, u64::MAX,
        ];
        let mut pairs: Vec<(u64, Vec<u8>)> = values
            .iter()
            .map(|v| (*v, VarInt::<u64>(*v).encode()))
            .collect();
        pairs.sort_by(|a, b| a.1.cmp(&b.1));
        let sorted_by_bytes: Vec<u64> = pairs.into_iter().map(|(v, _)| v).collect();
        let mut sorted_by_value = values.clone();
        sorted_by_value.sort_unstable();
        assert_eq!(sorted_by_bytes, sorted_by_value);
    }

    #[test]
    fn decode_reports_bytes_consumed() {
        let enc = VarInt::<u32>(300u32).encode();
        let (v, n) = u32::varint_decode(&enc);
        assert_eq!(v, 300);
        assert_eq!(n, enc.len());
    }

    #[test]
    #[should_panic(expected = "truncated")]
    fn truncated_frame_panics() {
        let _ = u64::varint_decode(&[0xFF]);
    }
}
