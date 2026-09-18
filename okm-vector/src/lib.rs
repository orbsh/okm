//! okm-vector — vector search built on okm-core's single-value function
//! indexes and junction entries.
//!
//! An embedding is a row's derived value: `func(embed)` returns one
//! fixed-width byte string — the classic single-value function index,
//! one entry per row. The store's prefix scan over those bytes gives
//! exact match and bucket recall for free; anything distance-based is
//! consumer-side algorithm work, expressed through the same two
//! primitives (ordered scans and junction entries):
//!
//! - **coarse recall** — quantized bucket id as the data segment
//!   (`scan` the bucket = candidates), rerank by true distance;
//! - **ANN** — a neighbor graph stored as `JunctionEncode` entries,
//!   greedy search via `okm-query::walk`-style hops.
//!
//! The storage form of an embedding field is `okm_core::Vector<f32, N>`
//! (P3.5): fixed-width little-endian wire, hot-segment eligible. The
//! helpers below operate on the same byte contract.

use okm_core::Vector;

/// The frame payload of an embedding field — `Vector<f32>::encode_payload`
/// (count prefix + bare LE elements). Kept as a free function for the
/// function-index `func` path, which receives `&document` and returns one
/// byte string.
pub fn embed_payload(v: &Vector<f32>) -> Vec<u8> {
    v.encode_payload()
}

/// Decode an embedding's frame payload back to the typed Vector.
pub fn embed_from_payload(b: &[u8]) -> Vector<f32> {
    Vector::<f32>::decode_payload(b)
}

/// Squared euclidean distance on raw f32 slices — the rerank metric.
pub fn dist2(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y) as f64 * (x - y) as f64).sum()
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
    fn vector_payload_roundtrip() {
        let v: Vector<f32> = Vector::new(vec![0.25, -3.5, 1e6]);
        let w = embed_payload(&v);
        assert_eq!(w.len(), 4 + 12); // count prefix + 3 dims
        assert_eq!(embed_from_payload(&w), v);
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
