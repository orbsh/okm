//! Exact scan — the ground truth every ANN result is measured against, and
//! the right path while a corpus is small enough that a full pass fits the
//! latency budget.
//!
//! Storage is one flat `Vec<f32>` plus the key table, so a query streams a
//! contiguous block instead of doing one KV read per row. That is the same
//! layout the server would hold for a hot shard; here it is also what makes
//! "exact" a usable baseline in tests rather than a slow reference.

use crate::dist2;

pub struct ExactIndex {
    dim: usize,
    keys: Vec<Vec<u8>>,
    flat: Vec<f32>,
    live: Vec<bool>,
    live_count: usize,
}

impl ExactIndex {
    pub fn new(dim: usize) -> Self {
        ExactIndex {
            dim,
            keys: Vec::new(),
            flat: Vec::new(),
            live: Vec::new(),
            live_count: 0,
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn live(&self) -> usize {
        self.live_count
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Append one vector. A repeated key replaces in place (the exact path has
    /// no neighbours to invalidate, so no tombstone is needed).
    pub fn add(&mut self, key: Vec<u8>, v: &[f32]) {
        assert_eq!(v.len(), self.dim, "exact index dim mismatch");
        if let Some(pos) = self.keys.iter().position(|k| k == &key) {
            self.flat[pos * self.dim..(pos + 1) * self.dim].copy_from_slice(v);
            if !self.live[pos] {
                self.live[pos] = true;
                self.live_count += 1;
            }
            return;
        }
        self.keys.push(key);
        self.flat.extend_from_slice(v);
        self.live.push(true);
        self.live_count += 1;
    }

    pub fn remove(&mut self, key: &[u8]) -> bool {
        match self.keys.iter().position(|k| k == key) {
            Some(pos) if self.live[pos] => {
                self.live[pos] = false;
                self.live_count -= 1;
                true
            }
            _ => false,
        }
    }

    /// Exact top-k by squared euclidean distance (cosine-equivalent order for
    /// L2-normalized inputs).
    pub fn search(&self, q: &[f32], k: usize) -> Vec<(Vec<u8>, f32)> {
        assert_eq!(q.len(), self.dim, "exact index dim mismatch");
        let mut scored: Vec<(f32, usize)> = Vec::with_capacity(self.live_count);
        for (pos, key_live) in self.live.iter().enumerate() {
            if !key_live {
                continue;
            }
            let v = &self.flat[pos * self.dim..(pos + 1) * self.dim];
            scored.push((dist2(v, q) as f32, pos));
        }
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored
            .into_iter()
            .take(k)
            .map(|(d, pos)| (self.keys[pos].clone(), d))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_first_and_removal_hides() {
        let mut x = ExactIndex::new(3);
        x.add(b"a".to_vec(), &[0.0, 0.0, 0.0]);
        x.add(b"b".to_vec(), &[1.0, 0.0, 0.0]);
        x.add(b"c".to_vec(), &[5.0, 0.0, 0.0]);
        let hits = x.search(&[0.9, 0.0, 0.0], 2);
        assert_eq!(hits[0].0, b"b".to_vec());
        assert_eq!(hits[1].0, b"a".to_vec());
        assert!(x.remove(b"b"));
        assert_eq!(x.search(&[0.9, 0.0, 0.0], 1)[0].0, b"a".to_vec());
        assert_eq!(x.live(), 2);
    }
}
