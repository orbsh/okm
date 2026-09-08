//! `VarInt<T>` — LEB128 variable-length wrapper for unsigned fields.

/// LEB128 (little-endian base-128) encoding: each byte carries 7 payload
/// bits; the high bit marks continuation. Small values shrink (1 byte for
/// < 128, 2 for < 16 384, …), the frame's own `len u32` absorbs the
/// variability so decode needs no out-of-band width.
///
/// Unsigned only (`u16`/`u32`/`u64`; `u8` is already minimal-width).
/// Values are the full integer domain — LEB128 of `u64::MAX` is 10 bytes.
pub trait VarIntEnc: Copy + Sized {
    /// Maximum encoded width for this type (LEB128 bound).
    const MAX_WIDTH: usize;
    /// Write the LEB128 encoding to `buf`.
    fn varint_encode(self, buf: &mut Vec<u8>);
    /// Decode from the front of `b`; returns the value and bytes consumed.
    /// Panics on truncated input or a 10th continuation byte.
    fn varint_decode(b: &[u8]) -> (Self, usize);
}

macro_rules! impl_varint {
    ($t:ty, $maxw:literal) => {
        impl VarIntEnc for $t {
            const MAX_WIDTH: usize = $maxw;
            fn varint_encode(self, buf: &mut Vec<u8>) {
                let mut v = self;
                loop {
                    let byte = (v & 0x7F) as u8;
                    v >>= 7;
                    if v == 0 {
                        buf.push(byte);
                        break;
                    }
                    buf.push(byte | 0x80);
                }
            }
            fn varint_decode(b: &[u8]) -> (Self, usize) {
                let mut v: $t = 0;
                let mut shift = 0u32;
                let mut i = 0usize;
                loop {
                    let byte = *b.get(i).expect("VarInt: truncated frame");
                    v |= ((byte & 0x7F) as $t) << shift;
                    i += 1;
                    if byte & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                    assert!(i < $maxw, "VarInt: continuation byte past max width");
                }
                (v, i)
            }
        }
    };
}

impl_varint!(u16, 3);
impl_varint!(u32, 5);
impl_varint!(u64, 10);

/// Newtype the derive macros recognize in field position (`VarInt<u32>`).
/// Field declarations stay plain `T`; the wrapper is applied at encode
/// time and removed at decode time, so user code keeps working with raw
/// values. Wire is variable length: `FieldDesc::width` is 0 and the TLV
/// frame's `len` is authoritative — same regime as `String`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VarInt<T: VarIntEnc>(pub T);

impl<T: VarIntEnc> VarInt<T> {
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
    fn leb128_shapes() {
        assert_eq!(VarInt::<u32>(0).encode(), vec![0x00]);
        assert_eq!(VarInt::<u32>(127).encode(), vec![0x7F]);
        assert_eq!(VarInt::<u32>(128).encode(), vec![0x80, 0x01]);
        assert_eq!(VarInt::<u32>(300).encode(), vec![0xAC, 0x02]);
        assert_eq!(VarInt::<u64>(u64::MAX).encode().len(), 10);
    }

    #[test]
    fn round_trip_domain_edges() {
        for v in [0u64, 1, 127, 128, 16_383, u32::MAX as u64, u64::MAX] {
            assert_eq!(VarInt::<u64>::decode(&VarInt(v).encode()).0, v);
        }
        for v in [0u16, u16::MAX] {
            assert_eq!(VarInt::<u16>::decode(&VarInt(v).encode()).0, v);
        }
    }

    #[test]
    fn decode_reports_bytes_consumed() {
        let (v, n) = u32::varint_decode(&[0xAC, 0x02]);
        assert_eq!((v, n), (300, 2));
    }

    #[test]
    #[should_panic(expected = "truncated")]
    fn truncated_frame_panics() {
        let _ = u64::varint_decode(&[0x80]);
    }
}
