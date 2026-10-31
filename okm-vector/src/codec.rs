//! Quantized node codes — what the graph stores per node instead of the raw
//! vector, and the two distance paths that ride on them.
//!
//! Two codecs, one contract: `encode` turns a vector into a code, `prepare`
//! turns a query into whatever the distance path needs, `distance` scores a
//! stored code against a prepared query. Both codecs rank by cosine-equivalent
//! order, so callers can hand the top candidates to an exact rerank without
//! translating metrics.
//!
//! - [`Codec::I8`] — scalar int8, `[norm f32 LE][dim i8]`. Cosine depends only
//!   on the integer codes: `cos = Σ c·d / (‖c‖‖d‖)` — the per-vector scales
//!   cancel, so the header carries the integer norm and nothing else. 4 bytes
//!   of overhead per vector, no training step.
//! - [`Codec::Pq`] — product quantization, `m` subspace codebooks of `ksub`
//!   centroids, one byte per subspace. Trained by k-means on a caller-supplied
//!   sample; distance is asymmetric (query stays exact) over reconstruction
//!   tables. 64 bytes for 512 dims at `m = 64`.
//!
//! Both are lossy on purpose: the graph exists to cut the candidate set, the
//! caller re-scores the survivors against the stored f32 vector.

// A stored code is this build's own bytes — every code in a graph was written by
// `encode` beside this reader — so the two panic arms below are the codec's abort
// (ADR-0040): a `Codec`/`Prepared` mismatch or a code shorter than its own 4-byte
// norm prefix means the row did not come from this build, and a distance that
// answered a made-up number would be folded straight back into the graph's edges.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

/// Codec tag bytes in the snapshot format.
const TAG_I8: u8 = 0;
const TAG_PQ: u8 = 1;

/// A stored node code.
pub type Code = Vec<u8>;

/// A query in the shape its codec's distance path wants.
pub enum Prepared {
    I8 {
        q: Vec<i8>,
        norm: f32,
    },
    /// `m × ksub` squared-distance table, filled once per query.
    Pq {
        tables: Vec<f32>,
    },
}

impl Prepared {
    /// Number of table entries — lets a caller size the scratch buffer.
    pub fn table_len(&self) -> usize {
        match self {
            Prepared::I8 { .. } => 0,
            Prepared::Pq { tables } => tables.len(),
        }
    }
}

/// A quantization scheme plus its parameters.
#[derive(Clone, Debug)]
pub enum Codec {
    I8(I8Spec),
    Pq(PqSpec),
}

#[derive(Clone, Debug)]
pub struct I8Spec {
    pub dim: usize,
}

#[derive(Clone, Debug)]
pub struct PqSpec {
    pub dim: usize,
    /// Subspaces (`dim` must divide evenly).
    pub m: usize,
    /// Centroids per subspace — 256 keeps a code at one byte per subspace.
    pub ksub: usize,
    /// `m × ksub × sub_dim` centroids, subspace-major then centroid-major.
    pub centroids: Vec<f32>,
}

impl PqSpec {
    pub fn sub_dim(&self) -> usize {
        self.dim / self.m
    }
}

impl Codec {
    /// Scalar int8, no training. The default: deterministic, and good enough
    /// while the corpus is small enough that recall is dominated by graph
    /// quality rather than code precision.
    pub fn i8(dim: usize) -> Self {
        Codec::I8(I8Spec { dim })
    }

    /// Train a product quantizer from a sample of vectors (at least `ksub` and
    /// ideally a few thousand). Returns `None` when the sample is too small or
    /// `dim` does not divide by `m`.
    pub fn pq(dim: usize, m: usize, sample: &[Vec<f32>], seed: u64) -> Option<Self> {
        if m == 0 || dim == 0 || !dim.is_multiple_of(m) {
            return None;
        }
        let sub = dim / m;
        let ksub = 256.min(sample.len());
        if ksub < 2 || sample.iter().any(|v| v.len() != dim) {
            return None;
        }
        let mut centroids = Vec::with_capacity(m * ksub * sub);
        let mut rng = SplitMix64::new(seed);
        for s in 0..m {
            let points: Vec<&[f32]> = sample.iter().map(|v| &v[s * sub..(s + 1) * sub]).collect();
            let chosen = kmeans(&points, ksub, sub, &mut rng, 12);
            centroids.extend_from_slice(&chosen);
        }
        Some(Codec::Pq(PqSpec {
            dim,
            m,
            ksub,
            centroids,
        }))
    }

    pub fn dim(&self) -> usize {
        match self {
            Codec::I8(s) => s.dim,
            Codec::Pq(s) => s.dim,
        }
    }

    /// Bytes per stored code (fixed width per codec).
    pub fn code_bytes(&self) -> usize {
        match self {
            Codec::I8(s) => 4 + s.dim,
            Codec::Pq(s) => s.m,
        }
    }

    pub fn encode(&self, v: &[f32]) -> Code {
        assert_eq!(v.len(), self.dim(), "codec dim mismatch");
        match self {
            Codec::I8(_) => encode_i8(v, self.dim()),
            Codec::Pq(s) => {
                let mut code = Vec::with_capacity(s.m);
                let sub = s.sub_dim();
                for (sidx, chunk) in v.chunks_exact(sub).enumerate() {
                    let base = sidx * s.ksub * sub;
                    code.push(
                        nearest(&s.centroids[base..base + s.ksub * sub], chunk, s.ksub, sub) as u8,
                    );
                }
                code
            }
        }
    }

    pub fn prepare(&self, q: &[f32]) -> Prepared {
        assert_eq!(q.len(), self.dim(), "codec dim mismatch");
        match self {
            Codec::I8(_) => {
                let (qi, _) = quantize_i8(q);
                let norm = (qi.iter().map(|c| (*c as f32) * (*c as f32)).sum::<f32>()).sqrt();
                Prepared::I8 { q: qi, norm }
            }
            Codec::Pq(s) => {
                let sub = s.sub_dim();
                let mut tables = vec![0f32; s.m * s.ksub];
                for (sidx, chunk) in q.chunks_exact(sub).enumerate() {
                    let base = sidx * s.ksub * sub;
                    for k in 0..s.ksub {
                        let c = &s.centroids[base + k * sub..base + (k + 1) * sub];
                        tables[sidx * s.ksub + k] = l2(chunk, c);
                    }
                }
                Prepared::Pq { tables }
            }
        }
    }

    /// Distance (smaller = closer) between a stored code and a prepared query.
    pub fn distance(&self, code: &[u8], p: &Prepared) -> f32 {
        match (self, p) {
            (Codec::I8(_), Prepared::I8 { q, norm }) => {
                let code_norm = code_norm_f32(code);
                if code_norm == 0.0 || *norm == 0.0 {
                    return 1.0;
                }
                let mut dot = 0f32;
                for (i, qi) in q.iter().enumerate() {
                    dot += i8_at(code, i) as f32 * *qi as f32;
                }
                1.0 - dot / (code_norm * *norm)
            }
            (Codec::Pq(s), Prepared::Pq { tables }) => {
                let mut d = 0f32;
                for (sidx, &c) in code.iter().enumerate().take(s.m) {
                    d += tables[sidx * s.ksub + c as usize];
                }
                d
            }
            _ => panic!("codec/prepared mismatch"),
        }
    }

    /// Prepare a query FROM a stored code — the insert path searches the graph
    /// with the new node's own code before any raw vector is in hand.
    pub fn prepare_code(&self, code: &[u8]) -> Prepared {
        match self {
            Codec::I8(s) => {
                let q: Vec<i8> = (0..s.dim).map(|i| i8_at(code, i)).collect();
                Prepared::I8 {
                    q,
                    norm: code_norm_f32(code),
                }
            }
            Codec::Pq(_) => Prepared::Pq {
                tables: self.tables_from_code(code),
            },
        }
    }

    /// PQ reconstruction tables for a stored code: `m × ksub` squared distances
    /// from that code's reconstruction to every centroid.
    fn tables_from_code(&self, code: &[u8]) -> Vec<f32> {
        let Codec::Pq(s) = self else {
            return Vec::new();
        };
        let sub = s.sub_dim();
        let mut tables = vec![0f32; s.m * s.ksub];
        for sidx in 0..s.m {
            let picked = sidx * s.ksub + code[sidx] as usize;
            let c = &s.centroids[picked * sub..(picked + 1) * sub];
            for k in 0..s.ksub {
                let cand = &s.centroids[(sidx * s.ksub + k) * sub..(sidx * s.ksub + k + 1) * sub];
                tables[sidx * s.ksub + k] = l2(c, cand);
            }
        }
        tables
    }

    /// Distance between two STORED codes — used when the graph prunes a
    /// neighbour list, where no query is in hand.
    pub fn distance_codes(&self, a: &[u8], b: &[u8]) -> f32 {
        match self {
            Codec::I8(_) => {
                let (na, nb) = (code_norm_f32(a), code_norm_f32(b));
                if na == 0.0 || nb == 0.0 {
                    return 1.0;
                }
                let mut dot = 0f32;
                for i in 0..self.dim() {
                    dot += i8_at(a, i) as f32 * i8_at(b, i) as f32;
                }
                1.0 - dot / (na * nb)
            }
            Codec::Pq(s) => {
                // Per-subspace centroid-to-centroid distance: O(dim), no
                // allocation. Pruning runs inside neighbour-list sorts, so this
                // path must stay cheap — building ADC tables per comparison was
                // three orders of magnitude slower in debug builds.
                let sub = s.sub_dim();
                let mut d = 0f32;
                for sidx in 0..s.m {
                    let a_c = &s.centroids[(sidx * s.ksub + a[sidx] as usize) * sub
                        ..(sidx * s.ksub + a[sidx] as usize + 1) * sub];
                    let b_c = &s.centroids[(sidx * s.ksub + b[sidx] as usize) * sub
                        ..(sidx * s.ksub + b[sidx] as usize + 1) * sub];
                    d += l2(a_c, b_c);
                }
                d
            }
        }
    }

    /// Snapshot encoding: tag + parameters (+ codebooks).
    pub fn encode_into(&self, w: &mut Vec<u8>) {
        match self {
            Codec::I8(s) => {
                w.push(TAG_I8);
                put_u32(w, s.dim as u32);
            }
            Codec::Pq(s) => {
                w.push(TAG_PQ);
                put_u32(w, s.dim as u32);
                put_u32(w, s.m as u32);
                put_u32(w, s.ksub as u32);
                for c in &s.centroids {
                    put_f32(w, *c);
                }
            }
        }
    }

    pub fn decode(r: &mut Reader<'_>) -> Option<Self> {
        let tag = r.u8()?;
        let dim = r.u32()? as usize;
        match tag {
            TAG_I8 => Some(Codec::I8(I8Spec { dim })),
            TAG_PQ => {
                let m = r.u32()? as usize;
                let ksub = r.u32()? as usize;
                if m == 0 || ksub == 0 || !dim.is_multiple_of(m) {
                    return None;
                }
                let n = m * ksub * (dim / m);
                let mut centroids = Vec::with_capacity(n);
                for _ in 0..n {
                    centroids.push(r.f32()?);
                }
                Some(Codec::Pq(PqSpec {
                    dim,
                    m,
                    ksub,
                    centroids,
                }))
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// int8 helpers

fn encode_i8(v: &[f32], dim: usize) -> Code {
    let (q, _) = quantize_i8(&v[..dim]);
    let norm = (q.iter().map(|c| (*c as f32) * (*c as f32)).sum::<f32>()).sqrt();
    let mut out = Vec::with_capacity(4 + dim);
    put_f32(&mut out, norm);
    out.extend(q.iter().map(|c| *c as u8));
    out
}

fn quantize_i8(v: &[f32]) -> (Vec<i8>, f32) {
    let max = v.iter().fold(0f32, |acc, x| acc.max(x.abs()));
    let scale = if max > 0.0 { 127.0 / max } else { 1.0 };
    (
        v.iter()
            .map(|x| (*x * scale).round().clamp(-127.0, 127.0) as i8)
            .collect(),
        scale,
    )
}

fn code_norm_f32(code: &[u8]) -> f32 {
    f32::from_le_bytes(code[0..4].try_into().unwrap())
}

fn i8_at(code: &[u8], i: usize) -> i8 {
    code[4 + i] as i8
}

fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn nearest(centroids: &[f32], v: &[f32], ksub: usize, sub: usize) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for k in 0..ksub {
        let c = &centroids[k * sub..(k + 1) * sub];
        let d = l2(v, c);
        if d < best_d {
            best_d = d;
            best = k;
        }
    }
    best
}

/// Deterministic k-means: `points` are `sub`-wide slices, returns
/// `k * sub` centroids. Seeded, fixed iteration count — a rebuilt index
/// reproduces byte-for-byte.
fn kmeans(points: &[&[f32]], k: usize, sub: usize, rng: &mut SplitMix64, iters: usize) -> Vec<f32> {
    let mut centroids = vec![0f32; k * sub];
    // Seed with distinct points (sample without replacement).
    let mut picked = vec![false; points.len()];
    for c in 0..k {
        let mut idx = (rng.next_u64() as usize) % points.len();
        while picked[idx] {
            idx = (idx + 1) % points.len();
        }
        picked[idx] = true;
        centroids[c * sub..(c + 1) * sub].copy_from_slice(points[idx]);
    }
    let mut assign = vec![0usize; points.len()];
    for _ in 0..iters {
        for (i, p) in points.iter().enumerate() {
            assign[i] = nearest(&centroids, p, k, sub);
        }
        let mut sums = vec![0f32; k * sub];
        let mut counts = vec![0usize; k];
        for (i, p) in points.iter().enumerate() {
            let c = assign[i];
            counts[c] += 1;
            for d in 0..sub {
                sums[c * sub + d] += p[d];
            }
        }
        for c in 0..k {
            if counts[c] == 0 {
                // Keep the old centroid: a dead cluster is a data property,
                // not a reason to move it to an arbitrary point.
                continue;
            }
            for d in 0..sub {
                centroids[c * sub + d] = sums[c * sub + d] / counts[c] as f32;
            }
        }
    }
    centroids
}

// ---------------------------------------------------------------------------
// Deterministic RNG — the graph structure must be reproducible from a seed.

#[derive(Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ---------------------------------------------------------------------------
// Little-endian primitives shared with the snapshot writer.

pub fn put_u16(w: &mut Vec<u8>, v: u16) {
    w.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u32(w: &mut Vec<u8>, v: u32) {
    w.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u64(w: &mut Vec<u8>, v: u64) {
    w.extend_from_slice(&v.to_le_bytes());
}
pub fn put_f32(w: &mut Vec<u8>, v: f32) {
    w.extend_from_slice(&v.to_le_bytes());
}

/// Bounds-checked reader over a snapshot blob.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    pub fn u8(&mut self) -> Option<u8> {
        let v = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    pub fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes(b.try_into().ok()?))
    }
    pub fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes(b.try_into().ok()?))
    }
    pub fn i32(&mut self) -> Option<i32> {
        let b = self.take(4)?;
        Some(i32::from_le_bytes(b.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        let b = self.take(8)?;
        Some(u64::from_le_bytes(b.try_into().ok()?))
    }
    pub fn f32(&mut self) -> Option<f32> {
        let b = self.take(4)?;
        Some(f32::from_le_bytes(b.try_into().ok()?))
    }
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.take(n)
    }
    pub fn left(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
}
