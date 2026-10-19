//! In-memory HNSW over quantized codes: build, append, search, tombstone.
//!
//! The graph is a **built artifact**, not stored state: nodes live in one
//! contiguous `Vec`, neighbour lists are `u32` ids, and the whole structure is
//! reproducible from `(seed, insertion order)`. That is what lets the server
//! keep it in RAM, rebuild it from the corpus, and ship it to query nodes as a
//! snapshot blob instead of writing tens of neighbour entries per insert.
//!
//! Division of labour with the caller:
//! - the caller owns the **key** (any opaque bytes: a primary key);
//! - the codec owns **memory per node** ([`Codec`]: int8 or product codes);
//! - this module owns **recall** — the search returns approximate top-k, and
//!   the caller re-scores those survivors against the stored f32 vector.
//!
//! Deletes are tombstones: the node keeps routing traffic (removing it from
//! every neighbour list would cost the whole graph) but never appears in a
//! result. Live/deleted ratio is the caller's rebuild trigger.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::codec::{Code, Codec, Prepared, Reader, SplitMix64, put_u16, put_u32};

/// Graph shape. `m` neighbours per upper layer, `m0` at layer 0 (denser, it is
/// where every search ends), `ef_construction` candidate width while linking.
#[derive(Clone, Debug)]
pub struct HnswParams {
    pub m: usize,
    pub m0: usize,
    pub ef_construction: usize,
    pub seed: u64,
}

impl Default for HnswParams {
    fn default() -> Self {
        // 16/32 at ef_c 100 is the standard 512-dim starting point: recall
        // above 0.95 on embeddings, ~1000 distance evaluations per search at
        // ef_search 64.
        HnswParams {
            m: 16,
            m0: 32,
            ef_construction: 100,
            seed: 0x5EED_1234_ABCD_0001,
        }
    }
}

/// One graph node. `levels[0]` is the dense layer every search traverses;
/// `levels[k]` exists only for nodes that drew level ≥ k.
#[derive(Clone, Debug)]
struct Node {
    key: Vec<u8>,
    code: Code,
    tomb: bool,
    levels: Vec<Vec<u32>>,
}

/// A total order on f32 distances (NaN sorts last rather than panicking).
#[derive(Copy, Clone, Debug, PartialEq)]
struct Dist(f32);

impl Eq for Dist {}
impl Ord for Dist {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}
impl PartialOrd for Dist {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone)]
pub struct Hnsw {
    codec: Codec,
    params: HnswParams,
    nodes: Vec<Node>,
    by_key: HashMap<Vec<u8>, u32>,
    entry: Option<u32>,
    top: usize,
    live: usize,
    rng: SplitMix64,
}

impl Hnsw {
    pub fn new(codec: Codec, params: HnswParams) -> Self {
        let seed = params.seed;
        Hnsw {
            codec,
            params,
            nodes: Vec::new(),
            by_key: HashMap::new(),
            entry: None,
            top: 0,
            live: 0,
            rng: SplitMix64::new(seed),
        }
    }

    pub fn codec(&self) -> &Codec {
        &self.codec
    }

    pub fn params(&self) -> &HnswParams {
        &self.params
    }

    /// Inserted nodes, tombstones included.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Nodes that can still appear in a result.
    pub fn live(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Insert (or replace) one key. Replacing tombstones the old node and
    /// inserts a fresh one — in-place code/neighbour edits would silently
    /// invalidate the edges that point at the old code.
    pub fn insert(&mut self, key: Vec<u8>, v: &[f32]) {
        let code = self.codec.encode(v);
        if let Some(old) = self.by_key.get(&key).copied()
            && !self.nodes[old as usize].tomb
        {
            self.nodes[old as usize].tomb = true;
            self.live -= 1;
        }
        let level = self.random_level();
        let id = self.nodes.len() as u32;
        self.nodes.push(Node {
            key: key.clone(),
            code: code.clone(),
            tomb: false,
            levels: vec![Vec::new(); level + 1],
        });
        self.by_key.insert(key, id);
        self.live += 1;

        let Some(mut ep) = self.entry else {
            self.entry = Some(id);
            self.top = level;
            return;
        };

        let p = self.codec.prepare_code(&code);
        for l in ((level + 1)..=self.top).rev() {
            ep = self.greedy(&p, ep, l);
        }
        let mut eps = vec![ep];
        for l in (0..=level.min(self.top)).rev() {
            let cands = self.search_layer(&p, &eps, self.params.ef_construction, l);
            let cap = if l == 0 {
                self.params.m0
            } else {
                self.params.m
            };
            let chosen: Vec<u32> = cands.iter().take(cap).map(|(_, u)| *u).collect();
            self.nodes[id as usize].levels[l] = chosen.clone();
            for &n in &chosen {
                self.link(n, id, l);
            }
            if let Some((_, best)) = cands.first() {
                eps = vec![*best];
            }
        }
        if level > self.top {
            self.top = level;
            self.entry = Some(id);
        }
    }

    /// Tombstone a key. Returns whether a live node was removed.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        match self.by_key.get(key).copied() {
            Some(id) if !self.nodes[id as usize].tomb => {
                self.nodes[id as usize].tomb = true;
                self.live -= 1;
                true
            }
            _ => false,
        }
    }

    /// Approximate top-k. Distances are the codec's (int8 cosine or PQ L2);
    /// the caller re-scores these keys against the stored f32 vectors.
    pub fn search(&self, q: &[f32], k: usize, ef: usize) -> Vec<(Vec<u8>, f32)> {
        let p = self.codec.prepare(q);
        self.search_prepared(&p, k, ef)
    }

    pub fn search_prepared(&self, p: &Prepared, k: usize, ef: usize) -> Vec<(Vec<u8>, f32)> {
        if k == 0 {
            return Vec::new();
        }
        let Some(mut ep) = self.entry else {
            return Vec::new();
        };
        for l in (1..=self.top).rev() {
            ep = self.greedy(p, ep, l);
        }
        let cands = self.search_layer(p, &[ep], ef.max(k), 0);
        let mut out = Vec::with_capacity(k);
        for (d, id) in cands {
            let n = &self.nodes[id as usize];
            if n.tomb {
                continue;
            }
            out.push((n.key.clone(), d));
            if out.len() == k {
                break;
            }
        }
        out
    }

    // -- internals ---------------------------------------------------------

    fn random_level(&mut self) -> usize {
        let m = self.params.m.max(1) as f64;
        let mut level = 0usize;
        while self.rng.f64() < 1.0 / m && level < 32 {
            level += 1;
        }
        level
    }

    /// One greedy hop sequence inside a single layer: walk to the locally
    /// closest neighbour until no neighbour improves.
    fn greedy(&self, p: &Prepared, mut ep: u32, layer: usize) -> u32 {
        let mut best = self.codec.distance(&self.nodes[ep as usize].code, p);
        loop {
            let mut moved = false;
            for &n in &self.nodes[ep as usize].levels[layer] {
                let d = self.codec.distance(&self.nodes[n as usize].code, p);
                if d < best {
                    best = d;
                    ep = n;
                    moved = true;
                }
            }
            if !moved {
                return ep;
            }
        }
    }

    /// Beam search in one layer: `ef` results, sorted ascending.
    fn search_layer(&self, p: &Prepared, eps: &[u32], ef: usize, layer: usize) -> Vec<(f32, u32)> {
        let mut visited: HashSet<u32> = HashSet::with_capacity(ef * 4);
        let mut cand: BinaryHeap<Reverse<(Dist, u32)>> = BinaryHeap::new();
        let mut top: BinaryHeap<(Dist, u32)> = BinaryHeap::new();
        for &e in eps {
            if !visited.insert(e) {
                continue;
            }
            let d = self.codec.distance(&self.nodes[e as usize].code, p);
            cand.push(Reverse((Dist(d), e)));
            top.push((Dist(d), e));
            if top.len() > ef {
                top.pop();
            }
        }
        while let Some(Reverse((d, u))) = cand.pop() {
            let worst = top.peek().map(|(Dist(x), _)| *x).unwrap_or(f32::INFINITY);
            if d.0 > worst && top.len() >= ef {
                break;
            }
            for &n in &self.nodes[u as usize].levels[layer] {
                if !visited.insert(n) {
                    continue;
                }
                let dn = self.codec.distance(&self.nodes[n as usize].code, p);
                if top.len() < ef || dn < worst {
                    cand.push(Reverse((Dist(dn), n)));
                    top.push((Dist(dn), n));
                    if top.len() > ef {
                        top.pop();
                    }
                }
            }
        }
        let mut out: Vec<(f32, u32)> = top.into_iter().map(|(Dist(d), u)| (d, u)).collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        out
    }

    /// Add `to` to `from`'s neighbour list at `layer`, pruning the list back to
    /// its cap by distance from `from`. Both directions are maintained because
    /// the graph is searched from either end.
    fn link(&mut self, from: u32, to: u32, layer: usize) {
        if from == to {
            return;
        }
        if self.nodes[from as usize].levels[layer].contains(&to) {
            return;
        }
        self.nodes[from as usize].levels[layer].push(to);
        let cap = if layer == 0 {
            self.params.m0
        } else {
            self.params.m
        };
        if self.nodes[from as usize].levels[layer].len() > cap {
            let base = self.nodes[from as usize].code.clone();
            let mut list = std::mem::take(&mut self.nodes[from as usize].levels[layer]);
            list.sort_by(|a, b| {
                let da = self
                    .codec
                    .distance_codes(&base, &self.nodes[*a as usize].code);
                let db = self
                    .codec
                    .distance_codes(&base, &self.nodes[*b as usize].code);
                da.total_cmp(&db).then(a.cmp(b))
            });
            list.truncate(cap);
            self.nodes[from as usize].levels[layer] = list;
        }
    }

    // -- snapshot ----------------------------------------------------------

    /// Inner node table (codec, params, entry, nodes) — the shard framing lives
    /// in [`crate::index`].
    pub fn encode_into(&self, w: &mut Vec<u8>) {
        self.codec.encode_into(w);
        put_u16(w, self.params.m as u16);
        put_u16(w, self.params.m0 as u16);
        put_u16(w, self.params.ef_construction as u16);
        w.extend_from_slice(&self.params.seed.to_le_bytes());
        w.extend_from_slice(&(self.entry.map(|e| e as i32).unwrap_or(-1)).to_le_bytes());
        put_u16(w, self.top as u16);
        put_u32(w, self.live as u32);
        put_u32(w, self.nodes.len() as u32);
        for n in &self.nodes {
            put_u16(w, n.key.len() as u16);
            w.extend_from_slice(&n.key);
            put_u32(w, n.code.len() as u32);
            w.extend_from_slice(&n.code);
            w.push(n.tomb as u8);
            w.push(n.levels.len() as u8);
            for level in &n.levels {
                put_u16(w, level.len() as u16);
                for &id in level {
                    put_u32(w, id);
                }
            }
        }
    }

    pub fn decode(r: &mut Reader<'_>) -> Option<Self> {
        let codec = Codec::decode(r)?;
        let m = r.u16()? as usize;
        let m0 = r.u16()? as usize;
        let ef_construction = r.u16()? as usize;
        let seed = r.u64()?;
        let entry = match r.i32()? {
            -1 => None,
            v if v >= 0 => Some(v as u32),
            _ => return None,
        };
        let top = r.u16()? as usize;
        let live = r.u32()? as usize;
        let count = r.u32()? as usize;
        let mut nodes = Vec::with_capacity(count);
        let mut by_key = HashMap::with_capacity(count);
        for id in 0..count {
            let klen = r.u16()? as usize;
            let key = r.bytes(klen)?.to_vec();
            let clen = r.u32()? as usize;
            let code = r.bytes(clen)?.to_vec();
            let tomb = r.u8()? != 0;
            let nlevels = r.u8()? as usize;
            let mut levels = Vec::with_capacity(nlevels);
            for _ in 0..nlevels {
                let n = r.u16()? as usize;
                let mut list = Vec::with_capacity(n);
                for _ in 0..n {
                    list.push(r.u32()?);
                }
                levels.push(list);
            }
            by_key.insert(key.clone(), id as u32);
            nodes.push(Node {
                key,
                code,
                tomb,
                levels,
            });
        }
        Some(Hnsw {
            codec,
            params: HnswParams {
                m,
                m0,
                ef_construction,
                seed,
            },
            nodes,
            by_key,
            entry,
            top,
            live,
            rng: SplitMix64::new(seed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Codec;

    fn vecs(n: usize, dim: usize) -> Vec<(Vec<u8>, Vec<f32>)> {
        let mut rng = SplitMix64::new(7);
        (0..n)
            .map(|i| {
                let mut v: Vec<f32> = (0..dim).map(|_| (rng.f64() as f32) - 0.5).collect();
                normalize(&mut v);
                (format!("k{i:05}").into_bytes(), v)
            })
            .collect()
    }

    fn normalize(v: &mut [f32]) {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            for x in v.iter_mut() {
                *x /= n;
            }
        }
    }

    fn exact(rows: &[(Vec<u8>, Vec<f32>)], q: &[f32], k: usize) -> Vec<Vec<u8>> {
        let mut scored: Vec<(f32, &Vec<u8>)> = rows
            .iter()
            .map(|(key, v)| {
                (
                    v.iter().zip(q).map(|(a, b)| (a - b) * (a - b)).sum::<f32>(),
                    key,
                )
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.into_iter().take(k).map(|(_, k)| k.clone()).collect()
    }

    #[test]
    fn recall_against_exact() {
        let dim = 64;
        let rows = vecs(400, dim);
        let mut g = Hnsw::new(
            Codec::i8(dim),
            HnswParams {
                m: 12,
                m0: 24,
                ef_construction: 64,
                seed: 42,
            },
        );
        for (key, v) in &rows {
            g.insert(key.clone(), v);
        }
        assert_eq!(g.live(), rows.len());

        let mut hits = 0usize;
        let queries = 40;
        for qi in 0..queries {
            let mut q: Vec<f32> = (0..dim)
                .map(|d| ((qi * 31 + d * 7) % 97) as f32 / 97.0 - 0.5)
                .collect();
            normalize(&mut q);
            let truth = exact(&rows, &q, 10);
            let got: HashSet<Vec<u8>> = g.search(&q, 10, 48).into_iter().map(|(k, _)| k).collect();
            hits += truth.iter().filter(|k| got.contains(*k)).count();
        }
        let recall = hits as f64 / (queries * 10) as f64;
        assert!(recall >= 0.85, "recall@10 = {recall:.3}");
    }

    #[test]
    fn determinism_from_seed() {
        let dim = 32;
        let rows = vecs(150, dim);
        let build = || {
            let mut g = Hnsw::new(Codec::i8(dim), HnswParams::default());
            for (key, v) in &rows {
                g.insert(key.clone(), v);
            }
            let mut w = Vec::new();
            g.encode_into(&mut w);
            w
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn tombstone_hides_from_results() {
        let dim = 16;
        let rows = vecs(50, dim);
        let mut g = Hnsw::new(Codec::i8(dim), HnswParams::default());
        for (key, v) in &rows {
            g.insert(key.clone(), v);
        }
        let q = rows[0].1.clone();
        assert!(g.search(&q, 5, 32).iter().any(|(k, _)| k == &rows[0].0));
        assert!(g.remove(&rows[0].0));
        assert!(!g.search(&q, 5, 32).iter().any(|(k, _)| k == &rows[0].0));
        assert_eq!(g.live(), rows.len() - 1);
    }

    #[test]
    fn snapshot_roundtrip_preserves_search() {
        let dim = 32;
        let rows = vecs(200, dim);
        let mut g = Hnsw::new(Codec::i8(dim), HnswParams::default());
        for (key, v) in &rows {
            g.insert(key.clone(), v);
        }
        let q = rows[3].1.clone();
        let before = g.search(&q, 10, 48);

        let mut blob = Vec::new();
        g.encode_into(&mut blob);
        let mut r = Reader::new(&blob);
        let back = Hnsw::decode(&mut r).expect("decode");
        assert_eq!(r.left(), 0);
        assert_eq!(back.len(), g.len());
        assert_eq!(back.search(&q, 10, 48), before);
    }
}
