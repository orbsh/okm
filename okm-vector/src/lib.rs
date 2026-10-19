//! okm-vector — vector search over okm-core's single-value function indexes.
//!
//! An embedding is a row's derived value: `func(embed)` returns one
//! fixed-width byte string — the classic single-value function index, one entry
//! per row. The storage form is `okm_core::Vector<f32>` (P3.5): a
//! `[count u32 BE][bare LE elements]` frame, no dimension in the type.
//!
//! Three layers, each optional:
//!
//! - [`ExactIndex`] — one contiguous `Vec<f32>`, full scan, exact. The right
//!   answer while the corpus is small enough that a query's byte volume fits
//!   the budget, and the ground truth the approximate path is measured against.
//! - [`Hnsw`] — an in-memory graph over quantized codes ([`Codec::I8`] or
//!   [`Codec::Pq`]). Built, never stored per node: reproducible from
//!   `(seed, insertion order)`, so it can be rebuilt from the corpus or shipped
//!   as a blob.
//! - [`VectorIndex`] — the store-facing handle: shards, the fan-out search and
//!   the one-blob-per-shard snapshot under [`VECTOR_INDEX_SLOT`].
//!
//! What this crate deliberately does **not** do:
//!
//! - **No permission filtering.** The graph ranks by distance only. Access
//!   control (reachability in the graph layer) is the caller's, applied by
//!   over-fetching `k' > k` and filtering downstream — filtering inside an ANN
//!   either loses recall or walks nodes the caller may not see.
//! - **No per-row writes.** The index is not a declaration and owns no slotted
//!   entry per row; the corpus stays in the document payload and the snapshot
//!   is a cache that can be dropped at any time.
//! - **No exact rerank.** The graph returns code-distance candidates;
//!   re-scoring them against the stored f32 vectors is a caller decision, and
//!   the only way to keep the code size low.
//!
//! Consistency is explicit: the caller tells the index about writes
//! ([`VectorIndex::add`] / [`VectorIndex::remove`]) or rebuilds it from a scan.
//! There is no hidden invalidation hook, and a snapshot's staleness is whatever
//! the rebuild cadence makes it.

pub mod codec;
pub mod exact;
pub mod hnsw;
pub mod index;

pub use codec::{Code, Codec, Prepared};
pub use exact::ExactIndex;
pub use hnsw::{Hnsw, HnswParams};
pub use index::{VECTOR_INDEX_SLOT, VectorIndex};

use okm_core::Vector;

/// The frame payload of a `Vector<f32>` field — `Vector::encode_payload`
/// (count prefix + bare LE elements). Kept as a free function for the
/// function-index `func` path, which receives `&document` and returns one
/// byte string. Naming note: this is a CODEC, not an embedder — the crate
/// never computes a vector.
pub fn vector_payload(v: &Vector<f32>) -> Vec<u8> {
    v.encode_payload()
}

/// Decode a `Vector<f32>` frame payload back to the typed vector.
pub fn vector_from_payload(b: &[u8]) -> Vector<f32> {
    Vector::<f32>::decode_payload(b)
}

/// Squared euclidean distance on raw f32 slices — the rerank metric. For
/// L2-normalized vectors this orders exactly like cosine.
pub fn dist2(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) as f64 * (x - y) as f64)
        .sum()
}

/// Quantized bucket id: one `i16 BE` per dimension, so byte order equals
/// bucket order and a bucket prefix stays a valid scan dimension.
///
/// Scalar bucketing is the cheap recall path (one prefix scan per probe); it is
/// deliberately lossy and meant for coarse candidate generation, never for
/// ranking. `step` is the bucket width in value units.
pub fn bucket_id(v: &[f32], step: f32) -> Vec<u8> {
    assert!(step > 0.0, "bucket step must be positive");
    let mut out = Vec::with_capacity(v.len() * 2);
    for f in v {
        let bucket = (f / step).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        out.extend_from_slice(&bucket.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_payload_roundtrip() {
        let v: Vector<f32> = Vector::new(vec![0.25, -3.5, 1e6]);
        let w = vector_payload(&v);
        assert_eq!(w.len(), 4 + 12); // count prefix + 3 dims
        assert_eq!(vector_from_payload(&w), v);
    }

    #[test]
    fn distance_squared() {
        assert_eq!(dist2(&[0.0, 0.0], &[3.0, 4.0]), 25.0);
    }

    #[test]
    fn bucket_order_follows_value_order() {
        // One-dim: bucket 5 < bucket 9, and BE byte order agrees — the
        // prefix-scan dimension stays meaningful.
        assert!(bucket_id(&[5.1], 1.0) < bucket_id(&[9.7], 1.0));
        // Fixed width: two dims are four bytes, regardless of magnitude.
        assert_eq!(bucket_id(&[0.0, 1e5], 1.0).len(), 4);
    }
}
