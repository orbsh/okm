//! `Reverse<T>` — descending sort-order wrapper for fixed-width integer
//! fields.
//!
//! Wrapping a field in `Reverse<T>` bit-flips its wire encoding so the
//! encoded bytes sort in DESCENDING value order. On an LSM KV (prefix
//! ordered), a `Reverse<u64>` timestamp as an index prefix therefore makes
//! the first scan hit the newest record — newest-first without any
//! application-side sort.
//!
//! Applicability is a compile-time whitelist ([`Reversible`], implemented
//! for the eight fixed-width integer types). Floats are excluded by
//! construction: IEEE-754 total order cannot be recovered by a fixed
//! bit-flip (sign/magnitude layout); quantize first (`Quant<T>` wrapper,
//! future) then wrap.
//!
//! One annotation, every destination: the derive macros apply the wrapper
//! to the key encoding, the TLV payload value, and any index-carried copy —
//! they all share the same declaration-order bytes.

/// Types whose wire encoding can be bit-flipped into a descending order.
///
/// Fixed-width integers only. Unsigned types flip directly (bitwise NOT is
/// order-reversing on BE bytes). Signed types first map into the unsigned
/// order-preserving domain (`x ^ sign mask`, i.e. bias by 2^(bits-1)), then
/// flip — the composition is still a plain per-byte transform on the wire.
pub trait Reversible: Copy + Sized {
    /// Encoded byte width (same as the plain encoding).
    const WIDTH: usize;
    /// Descending-order BE encoding.
    fn rev_encode(self) -> Vec<u8>;
    /// Inverse of [`Reversible::rev_encode`]; `b` is exactly `WIDTH` bytes.
    fn rev_decode(b: &[u8]) -> Self;
}

macro_rules! impl_rev_unsigned {
    ($t:ty, $w:literal) => {
        impl Reversible for $t {
            const WIDTH: usize = $w;
            fn rev_encode(self) -> Vec<u8> {
                (!self).to_be_bytes().to_vec()
            }
            fn rev_decode(b: &[u8]) -> Self {
                !<$t>::from_be_bytes(b.try_into().expect("rev width"))
            }
        }
    };
}

macro_rules! impl_rev_signed {
    ($t:ty, $u:ty, $w:literal) => {
        impl Reversible for $t {
            const WIDTH: usize = $w;
            fn rev_encode(self) -> Vec<u8> {
                // Order-preserving map into the unsigned domain, then flip.
                let biased = (self as $u) ^ (1 << ($w * 8 - 1));
                (!biased).to_be_bytes().to_vec()
            }
            fn rev_decode(b: &[u8]) -> Self {
                let flipped = !<$u>::from_be_bytes(b.try_into().expect("rev width"));
                ((flipped ^ (1 << ($w * 8 - 1))) as $t)
            }
        }
    };
}

impl_rev_unsigned!(u8, 1);
impl_rev_unsigned!(u16, 2);
impl_rev_unsigned!(u32, 4);
impl_rev_unsigned!(u64, 8);
impl_rev_signed!(i8, u8, 1);
impl_rev_signed!(i16, u16, 2);
impl_rev_signed!(i32, u32, 4);
impl_rev_signed!(i64, u64, 8);

/// Newtype the derive macros recognize in field position (`Reverse<u64>`).
/// Field declarations stay plain `T`; the wrapper is applied at encode time
/// and removed at decode time, so user code keeps working with raw values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Reverse<T: Reversible>(pub T);

impl<T: Reversible> Reverse<T> {
    /// Descending-order BE encoding of the wrapped value.
    pub fn encode(&self) -> Vec<u8> {
        self.0.rev_encode()
    }
    /// Inverse of [`Reverse::encode`]; `b` is exactly `T::WIDTH` bytes.
    pub fn decode(b: &[u8]) -> Self {
        Reverse(T::rev_decode(b))
    }
}
