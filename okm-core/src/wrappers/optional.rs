//! `Option<T>` — fixed-width presence wrapper for declared fields
//! (PLAN Phase 9, ADR-0012 wrapper family).
//!
//! Wire layout: `[present u8][T wire bytes]` — total width is
//! `1 + T::WIDTH`, so an optional field stays **hot-segment eligible**
//! (O(1) offsets, same discipline as every fixed-width kind). `None`
//! zero-fills the value bytes; `Some(0)` and `None` are byte-distinct.
//!
//! Why not plain `Option<T>` in field position: the derive's fixed-width
//! pipeline needs a wrapper contract (wire width, default, encode/decode)
//! exactly like `Enum<T>`/`Offset<T>`/`VarInt<T>`. `Optional` also
//! inherits the value's own semantics — a missing field and a field set
//! to zero are different facts.
//!
//! Orthogonality with `#[ok_default]`: the default decides what a
//! payload written by an older layout version decodes to; `Option`
//! decides presence *within* a payload. A missing pre-v2 field decodes
//! to `Optional(None)` unless `#[ok_default(Some(x))]` says otherwise.

/// The presence wrapper. `T` is any fixed-width codec field
/// (`u8..u64`, `[u8; N]`, `Enum<E>`, another wrapper...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Optional<T>(pub Option<T>);

impl<T: Default> Default for Optional<T> {
    fn default() -> Self {
        Optional(None)
    }
}

/// Wire width of `Optional<T>`: 1 presence byte + T's fixed width.
/// The derive reads this through `FieldDesc::width`.
pub trait WireWidth {
    const WIDTH: usize;
}

/// The encode/decode contract the derive calls per field. Separate from
/// `WireWidth` so the trait bounds stay minimal and readable.
pub trait OptionalEnc: WireWidth + Sized + Copy + PartialEq + std::fmt::Debug {
    /// Fixed-width BE bytes of the inner value (None → zero-filled).
    fn enc_wire(&self, buf: &mut Vec<u8>);
    /// Inverse of [`Self::enc_wire`]; `present == 0` yields None.
    fn dec_wire(present: u8, wire: &[u8]) -> Self;
}

impl<T: OptionalEnc + WireWidth> WireWidth for Optional<T> {
    const WIDTH: usize = 1 + T::WIDTH;
}
impl<T: OptionalEnc + WireWidth> OptionalEnc for Optional<T> {
    fn enc_wire(&self, buf: &mut Vec<u8>) {
        match &self.0 {
            Some(v) => {
                buf.push(1);
                v.enc_wire(buf);
            }
            None => {
                buf.push(0);
                buf.extend_from_slice(&vec![0u8; T::WIDTH]);
            }
        }
    }
    fn dec_wire(present: u8, wire: &[u8]) -> Self {
        if present == 0 {
            Optional(None)
        } else {
            Optional(Some(T::dec_wire(1, wire)))
        }
    }
}

macro_rules! impl_optional_prim {
    ($t:ty, $w:literal) => {
        impl WireWidth for $t {
            const WIDTH: usize = $w;
        }
        impl OptionalEnc for $t {
            fn enc_wire(&self, buf: &mut Vec<u8>) {
                buf.extend_from_slice(&self.to_be_bytes());
            }
            fn dec_wire(present: u8, wire: &[u8]) -> Self {
                let mut b = [0u8; $w];
                b.copy_from_slice(wire);
                let _ = present; // Some path: the caller already checked
                <$t>::from_be_bytes(b)
            }
        }
    };
}

impl_optional_prim!(u8, 1);
impl_optional_prim!(u16, 2);
impl_optional_prim!(u32, 4);
impl_optional_prim!(u64, 8);

impl<const N: usize> WireWidth for [u8; N] {
    const WIDTH: usize = N;
}
impl<const N: usize> OptionalEnc for [u8; N] {
    fn enc_wire(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self);
    }
    fn dec_wire(_present: u8, wire: &[u8]) -> Self {
        wire.try_into().expect("Optional<[u8; N]> wire width fixed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_and_some_zero_are_byte_distinct() {
        let mut none = Vec::new();
        Optional::<u64>(None).enc_wire(&mut none);
        assert_eq!(none.len(), 9);
        assert_eq!(none[0], 0);
        assert!(none[1..].iter().all(|&b| b == 0));

        let mut zero = Vec::new();
        Optional::<u64>(Some(0)).enc_wire(&mut zero);
        assert_eq!(zero.len(), 9);
        assert_eq!(zero[0], 1);
        assert!(zero[1..].iter().all(|&b| b == 0));

        // Byte-distinct: the presence byte differs.
        assert_ne!(none[0], zero[0]);
    }

    #[test]
    fn round_trips() {
        for v in [None, Some(0u32), Some(u32::MAX)] {
            let mut buf = Vec::new();
            Optional::<u32>(v).enc_wire(&mut buf);
            let back = Optional::<u32>::dec_wire(buf[0], &buf[1..]);
            assert_eq!(back, Optional::<u32>(v));
            assert_eq!(buf.len(), 5);
        }
        let mut buf = Vec::new();
        Optional::<[u8; 4]>(Some([1, 2, 3, 4])).enc_wire(&mut buf);
        let back = Optional::<[u8; 4]>::dec_wire(buf[0], &buf[1..]);
        assert_eq!(back, Optional::<[u8; 4]>(Some([1, 2, 3, 4])));
    }

    #[test]
    fn default_is_none() {
        assert_eq!(Optional::<u16>::default(), Optional(None));
    }
}
