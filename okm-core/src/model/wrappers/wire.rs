//! Wire primitives shared by every OKM frame format — the single
//! implementation of the variable-length integer (P1's prefix-monotonic
//! encoding) that the field wrapper, the dynamic-segment frame headers,
//! and the tooling all call. No second codec anywhere.
//!
//! Two families live here:
//!
//! - **VarIntEnc** (per-type trait): integer values, used where the value
//!   ITSELF is the datum (`VarInt<T>` field wrapper, tooling). Byte
//!   order = value order, so these are index-segment legal.
//! - **wire_len** (free functions): frame LENGTH prefixes — how many
//!   payload bytes follow. Lengths are never compared or sorted, so
//!   they reuse the same encoding via the u64 impl without the
//!   ordering contract mattering.

/// Append `len` as a prefix-monotonic varint (the frame-length prefix
/// codec — same layout as `VarInt<u64>`, no ordering contract needed).
#[inline]
pub fn put_len(buf: &mut Vec<u8>, len: usize) {
    let v = len as u64;
    let w = width_of(v);
    if w == 9 {
        buf.push(0xFF);
        buf.extend_from_slice(&v.to_be_bytes());
        return;
    }
    let payload_bits = (w - 1) * 8 + (8 - w);
    buf.push(prefix(w) | ((v >> ((w - 1) * 8)) as u8 & (0xFF >> w)));
    let low = v & ((1u64 << ((w - 1) * 8)) - 1);
    let lb = low.to_be_bytes();
    buf.extend_from_slice(&lb[8 - (w - 1)..]);
}

/// Read a varint length prefix from the front of `b`; returns
/// `(len, bytes consumed)`. `None` on truncated input.
#[inline]
pub fn take_len(b: &[u8]) -> Option<(usize, usize)> {
    let w = prefix_width(*b.first()?);
    if b.len() < w {
        return None;
    }
    let v: u64 = if w == 9 {
        u64::from_be_bytes(b[1..9].try_into().ok()?)
    } else {
        let payload_bits = (w - 1) * 8 + (8 - w);
        let hi = (b[0] & (0xFF >> w)) as u64;
        let mut low: u64 = 0;
        for i in 1..w {
            low = (low << 8) | b[i] as u64;
        }
        (hi << (payload_bits - (8 - w))) | low
    };
    usize::try_from(v).ok().map(|l| (l, w))
}

/// Internal helpers shared by the trait impls (single width logic).

/// Total encoded width for the first byte of a varint.
#[inline]
pub fn prefix_width(first: u8) -> usize {
    first.leading_ones() as usize + 1
}

/// Prefix byte for width `w` (1..=9): (w-1) leading ones + a 0 bit,
/// w=9 -> 0xFF.
#[inline]
pub fn prefix(w: usize) -> u8 {
    if w >= 9 {
        0xFF
    } else {
        ((!(0xFFFFu16 >> (w - 1))) >> 8) as u8
    }
}

/// Smallest width whose payload holds `v` (1..=9).
#[inline]
pub fn width_of(v: u64) -> usize {
    if v >= 1u64 << 56 {
        return 9;
    }
    let mut w = 1usize;
    loop {
        let payload_bits = (w - 1) * 8 + (8 - w);
        if v < 1u64 << payload_bits {
            return w;
        }
        w += 1;
    }
}
