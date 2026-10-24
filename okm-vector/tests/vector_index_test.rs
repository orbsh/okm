//! End-to-end checks over a real `VirtualStorage` implementation: the snapshot
//! blob set, shard routing/merge, and the two codecs.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Mutex;

use okm_core::VirtualStorage;
use okm_vector::{Codec, ExactIndex, HnswParams, VectorIndex};

/// Minimal ordered KV: enough to exercise `commit_batch` and the snapshot keys
/// exactly as an engine would.
struct MemStore {
    map: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl MemStore {
    fn new() -> Self {
        MemStore {
            map: Mutex::new(BTreeMap::new()),
        }
    }
}

impl VirtualStorage for MemStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> okm_core::error::Result<()> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, value);
        Ok(())
    }

    fn get(&self, key: &[u8]) -> okm_core::error::Result<Option<Vec<u8>>> {
        Ok(self.map.lock().unwrap().get(key).cloned())
    }

    fn del(&self, key: &[u8]) -> okm_core::error::Result<()> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        Ok(())
    }

    fn scan_range(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> okm_core::error::Result<Vec<Vec<u8>>> {
        let map = self.map.lock().unwrap();
        let lower = Bound::Included(begin.to_vec());
        let upper = match end {
            Some(e) => Bound::Excluded(e.to_vec()),
            None => Bound::Unbounded,
        };
        Ok(map.range((lower, upper)).map(|(k, _)| k.clone()).collect())
    }
}

fn rng_rows(n: usize, dim: usize, seed: u64) -> Vec<(Vec<u8>, Vec<f32>)> {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    (0..n)
        .map(|i| {
            let mut v: Vec<f32> = (0..dim)
                .map(|_| (next() % 1000) as f32 / 1000.0 - 0.5)
                .collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
            (format!("row/{i:05}").into_bytes(), v)
        })
        .collect()
}

#[test]
fn snapshot_roundtrip_over_storage() {
    let ns = [7u8, 3u8];
    let rows = rng_rows(240, 32, 11);
    let mut idx = VectorIndex::new(ns, Codec::i8(32), HnswParams::default(), 4);
    for (key, v) in &rows {
        idx.add(key.clone(), v);
    }
    let q = rows[17].1.clone();
    let before = idx.search(&q, 10, 48);
    assert_eq!(before.len(), 10);

    let mut store = MemStore::new();
    idx.save(&mut store).expect("save");

    let back = VectorIndex::load(&store, ns)
        .expect("a store read")
        .expect("load");
    assert_eq!(back.shard_count(), 4);
    assert_eq!(back.len(), idx.len());
    assert_eq!(back.live(), idx.live());
    assert_eq!(back.search(&q, 10, 48), before);

    // A missing shard is a failed load, never a partial index.
    let mut store2 = MemStore::new();
    idx.save(&mut store2).expect("save");
    store2.del(&idx.slot_key(3)).unwrap();
    assert!(VectorIndex::load(&store2, ns).expect("a store read").is_none());
}

#[test]
fn shards_split_the_corpus_and_merge_results() {
    let ns = [1u8, 2u8];
    let rows = rng_rows(200, 16, 5);
    let mut idx = VectorIndex::new(ns, Codec::i8(16), HnswParams::default(), 3);
    for (key, v) in &rows {
        idx.add(key.clone(), v);
    }
    assert_eq!(idx.len(), 200);
    // Every shard got work, and no key landed twice (routing is deterministic).
    let q = rows[0].1.clone();
    let hits = idx.search(&q, 8, 32);
    assert!(hits.iter().any(|(k, _)| k == &rows[0].0));
    assert!(
        hits.windows(2).all(|w| w[0].1 <= w[1].1),
        "merged hits are sorted"
    );
}

#[test]
fn pq_codes_are_compact_and_rank_well() {
    let dim = 32;
    let m = 8;
    let rows = rng_rows(600, dim, 99);
    let sample: Vec<Vec<f32>> = rows.iter().take(512).map(|(_, v)| v.clone()).collect();
    let codec = Codec::pq(dim, m, &sample, 3).expect("train pq");
    assert_eq!(codec.code_bytes(), m, "one byte per subspace");

    let mut idx = VectorIndex::new([0, 1], codec, HnswParams::default(), 1);
    for (key, v) in &rows {
        idx.add(key.clone(), v);
    }
    let mut exact = ExactIndex::new(dim);
    for (key, v) in &rows {
        exact.add(key.clone(), v);
    }

    let mut hits = 0usize;
    let queries = 20;
    for i in 0..queries {
        let q = &rows[i * 7].1;
        let truth: Vec<Vec<u8>> = exact.search(q, 10).into_iter().map(|(k, _)| k).collect();
        let got: Vec<Vec<u8>> = idx.search(q, 10, 64).into_iter().map(|(k, _)| k).collect();
        hits += truth.iter().filter(|k| got.contains(k)).count();
    }
    let recall = hits as f64 / (queries * 10) as f64;
    assert!(recall >= 0.6, "PQ recall@10 = {recall:.3}");
}
