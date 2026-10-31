//! The store-facing handle: shards, the fan-out search, and the snapshot blob.
//!
//! A `VectorIndex` is deliberately **not** a collection declaration. It owns no
//! slot family, writes nothing per row, and never participates in `put` /
//! `delete`: the corpus stays where it already lives (the document's payload),
//! and the graph is rebuilt from it. What this handle adds is the piece a
//! server needs in front of that corpus —
//!
//! - **shards**: `hash(key) % n`, each a self-contained graph, so a query fans
//!   out over independent structures and the work divides by shard;
//! - **one blob per shard** under [`VECTOR_INDEX_SLOT`], so a second process
//!   starts from the built graph instead of re-scanning the corpus;
//! - **no permission coupling**: the graph ranks by distance only. Access
//!   checks belong to the caller (the graph layer, in the k10r shape) and are
//!   applied by over-fetching `k' > k` and filtering downstream.
//!
//! Staleness is explicit, not hidden: a snapshot is a cache of a moment. The
//! caller decides when to rebuild (write count, a timer) and swaps the whole
//! handle; there is no per-node invalidation to get wrong.

use okm_core::engine::storage::{Batch, VirtualStorage};

use crate::codec::{Codec, Reader, put_u16};
use crate::hnsw::{Hnsw, HnswParams};

/// Fixed face 0x0A holds the built graph per shard:
/// `[ns 2B][0x0A][shard u16 BE] → blob`.
///
/// It is a cache entry, not a row-derived one: nothing in the corpus points at
/// it, and discarding every blob loses only rebuild time. It sits directly after
/// the graph's four faces (0x06–0x09) with no slot left empty between them;
/// 0x0B–0x0F stay free for fixed faces.
pub const VECTOR_INDEX_SLOT: u8 = 0x0A;

const MAGIC: &[u8; 8] = b"OKMVEC1\0";
const VERSION: u16 = 1;

pub struct VectorIndex {
    ns: [u8; 2],
    shards: Vec<Hnsw>,
}

impl VectorIndex {
    pub fn new(ns: [u8; 2], codec: Codec, params: HnswParams, shard_count: usize) -> Self {
        assert!(shard_count >= 1, "at least one shard");
        let shards = (0..shard_count)
            .map(|_| Hnsw::new(codec.clone(), params.clone()))
            .collect();
        VectorIndex { ns, shards }
    }

    pub fn ns(&self) -> [u8; 2] {
        self.ns
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn codec(&self) -> &Codec {
        self.shards[0].codec()
    }

    /// Inserted nodes across shards (tombstones included).
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    /// Nodes that can still appear in a result.
    pub fn live(&self) -> usize {
        self.shards.iter().map(|s| s.live()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn add(&mut self, key: Vec<u8>, v: &[f32]) {
        let shard = self.shard_of(&key);
        self.shards[shard].insert(key, v);
    }

    pub fn remove(&mut self, key: &[u8]) -> bool {
        let shard = self.shard_of(key);
        self.shards[shard].remove(key)
    }

    /// Approximate top-k across every shard, merged by code distance. Returns
    /// `k'` candidates for the caller's exact rerank — the distances here are
    /// codec-level (int8 cosine or PQ L2), not the stored vectors'.
    pub fn search(&self, q: &[f32], k: usize, ef: usize) -> Vec<(Vec<u8>, f32)> {
        let mut hits: Vec<(Vec<u8>, f32)> = Vec::with_capacity(k * self.shards.len());
        for shard in &self.shards {
            hits.extend(shard.search(q, k, ef));
        }
        hits.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        hits.truncate(k);
        hits
    }

    // -- snapshot ----------------------------------------------------------

    /// Write every shard's blob in ONE batch (a reader never sees a half
    /// snapshot: either every shard is the new build or every shard is the old
    /// one).
    pub fn save<S: VirtualStorage + ?Sized>(&self, store: &mut S) -> Result<(), String> {
        let mut batch = Batch::default();
        for (id, shard) in self.shards.iter().enumerate() {
            let mut w = Vec::with_capacity(4096);
            w.extend_from_slice(MAGIC);
            put_u16(&mut w, VERSION);
            w.extend_from_slice(&self.ns);
            put_u16(&mut w, id as u16);
            put_u16(&mut w, self.shards.len() as u16);
            shard.encode_into(&mut w);
            batch.put(self.slot_key(id), w);
        }
        store.commit_batch(batch).map_err(|e| e.to_string())
    }

    /// Read the snapshot back. `Ok(None)` when any shard is missing or the blob
    /// set disagrees on shape — the caller then rebuilds from the corpus.
    /// `Err` is the store failing to answer at all, which is a different fact.
    pub fn load<S: VirtualStorage + ?Sized>(
        store: &S,
        ns: [u8; 2],
    ) -> Result<Option<Self>, String> {
        let Some(first) = store.get(&slot_key(ns, 0)).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let Some((shard_count, shard0)) = decode_shard(&first, ns, 0) else {
            return Ok(None);
        };
        let mut shards = Vec::with_capacity(shard_count);
        shards.push(shard0);
        for id in 1..shard_count {
            let Some(blob) = store.get(&slot_key(ns, id)).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            let Some((count, shard)) = decode_shard(&blob, ns, id) else {
                return Ok(None);
            };
            if count != shard_count {
                return Ok(None);
            }
            shards.push(shard);
        }
        Ok(Some(VectorIndex { ns, shards }))
    }

    pub fn slot_key(&self, shard: usize) -> Vec<u8> {
        slot_key(self.ns, shard)
    }

    fn shard_of(&self, key: &[u8]) -> usize {
        (fnv1a64(key) % self.shards.len() as u64) as usize
    }
}

fn slot_key(ns: [u8; 2], shard: usize) -> Vec<u8> {
    let mut k = Vec::with_capacity(5);
    k.extend_from_slice(&ns);
    k.push(VECTOR_INDEX_SLOT);
    k.extend_from_slice(&(shard as u16).to_be_bytes());
    k
}

/// `(shard_count, graph)`; the blob must be self-consistent with the key it
/// was read from.
fn decode_shard(blob: &[u8], ns: [u8; 2], shard: usize) -> Option<(usize, Hnsw)> {
    if blob.len() < 16 || &blob[0..8] != MAGIC {
        return None;
    }
    let mut r = Reader::new(&blob[8..]);
    if r.u16()? != VERSION {
        return None;
    }
    let blob_ns = [r.u8()?, r.u8()?];
    if blob_ns != ns {
        return None;
    }
    if r.u16()? as usize != shard {
        return None;
    }
    let shard_count = r.u16()? as usize;
    if shard_count == 0 || shard >= shard_count {
        return None;
    }
    let graph = Hnsw::decode(&mut r)?;
    if r.left() != 0 {
        return None;
    }
    Some((shard_count, graph))
}

/// FNV-1a, 64-bit — shard routing must not depend on `std`'s hasher, whose
/// output is not a stable contract across releases.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}
