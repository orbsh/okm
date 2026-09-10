//! okm-vector — vector search built on okm's single-value function
//! indexes and edge entries.
//!
//! An embedding is a row's derived value: `func(embed)` returns one
//! fixed-width byte string — the classic single-value function index,
//! one entry per row. The store's prefix scan over those bytes gives
//! exact match and bucket recall for free; anything distance-based is
//! consumer-side algorithm work, expressed through the same two
//! primitives (ordered scans and edge entries):
//!
//! - **coarse recall** — quantized bucket id as the data segment
//!   (`scan` the bucket = candidates), rerank by true distance;
//! - **ANN** — a neighbor graph stored as `EdgeEncode` double-written
//!   edges, greedy search via `okm-query::walk`-style hops.

/// Fixed-width little-endian encoding of an f32 vector — the value a
/// `func` returns. Fixed width (4 bytes per dim) preserves the
/// right-hand anchor discipline: the primary key cuts off the tail.
pub fn encode_f32s(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn decode_f32s(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// Squared euclidean distance on raw f32 slices — the rerank metric.
pub fn dist2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// Quantized bucket id: cast each dim to a fixed-step bucket, encode
/// BE — byte order equals bucket order, so the segment stays a valid
/// prefix-scan dimension and a bucket probe is one prefix scan.
pub fn bucket_id(v: &[f32], step: f32) -> Vec<u8> {
    let ids: Vec<u8> = v.iter().map(|f| (f / step) as i64 as u8).collect();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_roundtrip() {
        let v = vec![0.25f32, -3.5, 1e6];
        assert_eq!(decode_f32s(&encode_f32s(&v)), v);
        assert_eq!(encode_f32s(&v).len(), 12); // fixed width, 3 dims
    }

    #[test]
    fn distance_squared() {
        assert_eq!(dist2(&[0.0, 0.0], &[3.0, 4.0]), 25.0);
    }

    #[test]
    fn bucket_order_follows_value_order() {
        // One-dim: bucket 5 < bucket 9, and BE byte order agrees —
        // the prefix-scan dimension stays meaningful.
        assert!(bucket_id(&[5.1], 1.0)[0] < bucket_id(&[9.7], 1.0)[0]);
    }
}
