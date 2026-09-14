//! Storage abstraction: sync [`VirtualStorage`] trait + [`MockStore`] reference
//! implementation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};


/// Minimal KV engine interface (prefix scan returns the "suffix" of each key).
/// The `fjall` / `slatedb` features each provide an implementation; tests
/// use [`MockStore`].
pub trait VirtualStorage {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>);
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn del(&mut self, key: &[u8]);
    /// Prefix scan; returns each matching key's "suffix" (prefix removed).
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>>;
    /// Prefix scan returning `(key suffix, value)` pairs, in key order.
    /// Default derives from `scan_suffix` + `get` (two lookups per hit);
    /// engines override with a native pair scan when it matters.
    fn scan_suffix_kv(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.scan_suffix(prefix)
            .into_iter()
            .map(|sfx| {
                let full = [prefix, sfx.as_slice()].concat();
                let v = self.get(&full).unwrap_or_default();
                (sfx, v)
            })
            .collect()
    }
    /// Open a write batch: [`KvBatch`] entries accumulate here, one
    /// `commit()` = one engine-level WAL commit. This is the
    /// cross-collection atomicity primitive (ADR-0003): single-table
    /// writes are already atomic inside `Table::put`; primary + index
    /// across two assembly points share one batch and commit once.
    /// Default carrier is [`MemBatch`] (op list replayed by the engine
    /// trait's own put/del); engines with a native batch API (fjall
    /// `Batch`) override to wrap theirs — the single WAL commit is real,
    /// not a replay.
    fn batch(&mut self) -> MemBatch {
        MemBatch::default()
    }
    /// Commit a batch: one engine-level write over all accumulated ops.
    /// Default replays the op list through put/del (single-threaded, so
    /// the sequence is atomic within the caller's write order); engines
    /// with a native batch override this to hand the carrier to the
    /// engine's own commit (one real WAL write).
    fn commit_batch(&mut self, batch: MemBatch) -> Result<(), String> {
        for (k, v) in batch.ops {
            match v {
                Some(v) => self.put(k, v),
                None => self.del(&k),
            }
        }
        Ok(())
    }
}

/// An engine that can be genuinely shared across hosts and handles:
/// the `shared_handle()` view IS the same physical keyspace (fjall /
/// slatedb store handles are Arc-kernel and already behave this way).
/// `StorageHost` requires it — a host wraps the engine in an
/// `Arc<Mutex<S>>`, and "wrapped" must mean shared, not copied.
/// Deep-copy-Clone engines (test maps) deliberately do not implement
/// this: a copied engine behind two hosts would silently fork the
/// keyspace.
pub trait SharedVirtualStorage: VirtualStorage {
    /// A handle to the same physical engine. Cheap; shares all state.
    fn shared_handle(&self) -> Self;
}

/// Engine-agnostic write batch: accumulates put/delete operations that
/// commit together in one engine-level WAL write (ADR-0003).
pub trait KvBatch {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>);
    fn del(&mut self, key: &[u8]);
    /// Commit atomically: one WAL write over everything accumulated.
    fn commit(self) -> Result<(), String>;
}

/// The default batch carrier: an ordered op list. Commit is a no-op on
/// the carrier itself — the caller replays via the engine trait's own
/// put/del; see [`VirtualStorage::batch`].
#[derive(Default)]
pub struct MemBatch {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl KvBatch for MemBatch {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.ops.push((key, Some(value)));
    }
    fn del(&mut self, key: &[u8]) {
        self.ops.push((key.to_vec(), None));
    }
    fn commit(self) -> Result<(), String> {
        Ok(())
    }
}

/// In-memory engine for tests and development: an `Arc`-kernel map
/// (handle-clone semantics — clones SHARE the keyspace, matching how
/// real engine handles behave; memcmp order matches real engines).
#[derive(Default, Clone)]
pub struct MockStore {
    map: Arc<Mutex<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl MockStore {
    /// Snapshot the whole keyspace (test observation helper: iterate a
    /// consistent copy). Not a production API.
    pub fn snapshot(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.map.lock().expect("map lock").clone()
    }
    pub fn keys(&self) -> Vec<Vec<u8>> {
        self.map.lock().expect("map lock").keys().cloned().collect()
    }
}

impl VirtualStorage for MockStore {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.map.lock().expect("map lock").insert(key, value);
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.lock().expect("map lock").get(key).cloned()
    }
    fn del(&mut self, key: &[u8]) {
        self.map.lock().expect("map lock").remove(key);
    }
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.map
            .lock()
            .expect("map lock")
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k[prefix.len()..].to_vec())
            .collect()
    }
}

impl SharedVirtualStorage for MockStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}
