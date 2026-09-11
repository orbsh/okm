//! `Quant<T, P>` — fixed-point quantization wrapper for float fields.

/// Scaling factor for `Quant<f64, P>`: 10^P as f64, computed at compile
/// time (const fn powi is not const-stable, hence the match).
const fn pow10(p: u32) -> f64 {
    match p {
        0 => 1.0,
        1 => 10.0,
        2 => 100.0,
        3 => 1_000.0,
        4 => 10_000.0,
        5 => 100_000.0,
        6 => 1_000_000.0,
        7 => 10_000_000.0,
        8 => 100_000_000.0,
        _ => panic!("Quant: precision P must be 0..=8"),
    }
}

/// Quantize `f` to a fixed-point integer with `P` decimal places.
///
/// `x * 10^P` rounded to nearest; NaN and out-of-range results panic —
/// a declaration-time contract, consistent with the codec's fail-fast
/// stance. The resulting integer's wire is a plain fixed-width BE
/// encoding, which composes with `Reverse` (`Reverse<Quant<f64, 3>>`
/// gives a descending float order — the path IEEE-754 bit tricks
/// cannot provide).
///
/// Only `f64` is implemented: a `f32` wire would still be 8 bytes of
/// i64 unless P is tiny, so the narrower type buys nothing here.
pub fn quantize(f: f64, p: u32) -> i64 {
    let scaled = f * pow10(p);
    let r = scaled.round();
    assert!(
        r.is_finite() && r >= i64::MIN as f64 && r <= i64::MAX as f64,
        "Quant: value {f} out of i64 range at precision P={p}"
    );
    r as i64
}

/// Inverse of [`quantize`].
pub fn dequantize(w: i64, p: u32) -> f64 {
    w as f64 / pow10(p)
}

/// BE bytes of a quantized wire integer (the fixed-point encoding).
pub fn wire_to_be_bytes(w: i64) -> Vec<u8> {
    w.to_be_bytes().to_vec()
}

/// Newtype the derive macros recognize in field position
/// (`Quant<f64, 3>`). `P` is the decimal-places const parameter; the
/// wire is `i64` BE (8 bytes, same width regardless of P).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Quant<const P: u32>(pub f64);

impl<const P: u32> Quant<P> {
    pub fn new(v: f64) -> Self {
        Quant(v)
    }
    /// Fixed-point wire encoding.
    pub fn encode(&self) -> Vec<u8> {
        wire_to_be_bytes(quantize(self.0, P))
    }
    /// Inverse of [`Quant::encode`]; `b` is exactly 8 bytes.
    pub fn decode(b: &[u8]) -> Self {
        let mut wire = [0u8; 8];
        wire.copy_from_slice(&b[..8]);
        Quant(dequantize(i64::from_be_bytes(wire), P))
    }
    /// The underlying fixed-point integer (e.g. for sort keys).
    pub fn wire(&self) -> i64 {
        quantize(self.0, P)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Q3 = Quant<3>;

    #[test]
    fn quantize_round_trip() {
        for v in [0.0, 1.5, -2.25, 123.456, -0.0005, 1e9] {
            let back = Q3::decode(&Q3::new(v).encode()).0;
            // decoded equals the rounded fixed-point value
            assert_eq!(back, (v * 1000.0).round() / 1000.0);
        }
    }

    #[test]
    fn wire_is_stable_width_and_order_preserving() {
        let lo = Q3::new(1.5).encode();
        let hi = Q3::new(2.5).encode();
        assert_eq!(lo.len(), 8);
        assert!(lo < hi, "fixed-point keeps ascending order on BE bytes");
    }

    #[test]
    fn reverse_composition_descends_float_order() {
        // The documented path for descending floats: quantize then reverse.
        let lo = Quant::<3>::new(-1.5).wire();
        let hi = Quant::<3>::new(9.75).wire();
        let lo_enc = crate::Reverse(lo).encode();
        let hi_enc = crate::Reverse(hi).encode();
        assert!(lo_enc > hi_enc);
    }

    #[test]
    #[should_panic(expected = "out of i64 range")]
    fn overflow_panics() {
        let _ = Q3::new(1e19).encode();
    }
}
