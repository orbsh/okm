//! `Offset<T>` — based-encoding wrapper: store `value − base` instead of
//! the absolute value.
//!
//! Annotation form (field attribute, read by the derive macros):
//!
//! ```ignore
//! #[derive(RowEncode)]
//! #[kv_ref(K)]
//! struct Row {
//!     #[kv_offset(base = 1_700_000_000)]
//!     created: i64,     // stored as u32 offset from the epoch base
//! }
//! ```
//!
//! The base is a **static schema constant**: changing it changes the wire
//! format (old bytes decode as garbage), so it belongs in the declaration,
//! not in runtime state. Wire is a fixed-width `u32`; the arithmetic is
//! i64-checked and out-of-range values panic (fail-fast, matching the
//! codec family).

/// Checked offset arithmetic shared by the derive-generated code.
pub fn offset_encode(value: i64, base: i64) -> Vec<u8> {
    let off = value
        .checked_sub(base)
        .filter(|d| (0..=u32::MAX as i64).contains(d))
        .unwrap_or_else(|| {
            panic!("Offset: value {value} − base {base} outside u32 range")
        });
    (off as u32).to_be_bytes().to_vec()
}

/// Inverse of [`offset_encode`]; `b` is exactly 4 bytes.
pub fn offset_decode(b: &[u8], base: i64) -> i64 {
    let off = u32::from_be_bytes(b.try_into().expect("offset width")) as i64;
    base + off
}

/// Newtype the derive macros recognize in field position
/// (`Offset<i64>` paired with `#[kv_offset(base = N)]`). Field
/// declarations stay wrapped; `.0` is the absolute value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offset(pub i64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_shrinking() {
        let base = 1_700_000_000i64;
        let enc = offset_encode(1_700_000_500, base);
        assert_eq!(enc.len(), 4);
        assert_eq!(offset_decode(&enc, base), 1_700_000_500);
        // The point: near-base values use the full u32 domain instead of
        // needing a wider integer type.
        assert_eq!(offset_encode(base, base), vec![0, 0, 0, 0]);
    }

    #[test]
    fn negative_offsets_are_also_representable() {
        // A negative base makes signed domains fit: base = i64::MIN-style
        // anchors, offsets climb from there.
        let base = -1_000i64;
        let enc = offset_encode(-500, base);
        assert_eq!(offset_decode(&enc, base), -500);
    }

    #[test]
    #[should_panic(expected = "outside u32 range")]
    fn out_of_range_panics() {
        let _ = offset_encode(1_700_000_000, u32::MAX as i64 + 2);
    }

    #[test]
    #[should_panic(expected = "outside u32 range")]
    fn below_base_panics() {
        let _ = offset_encode(1_699_999_999, 1_700_000_000);
    }
}
