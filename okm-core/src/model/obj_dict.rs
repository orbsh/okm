//! Field-name dictionary (ADR-0012): the run-time, per-table,
//! bidirectional mapping between dynamic-field names and their frame
//! ids. Stored as two point-lookup tables inside the obj's ns segment:
//!
//! ```text
//! slot 2: [ns][2][field-id u16 BE]   → name bytes
//! slot 3: [ns][3][name bytes]        → field-id u16 BE
//! ```
//!
//! Discipline copied from the ns dictionary (ADR-0002): append-only —
//! ids are never reused, never renumbered; a name seen for the first
//! time on the write path claims the next free number (single-writer
//! allocation, serialized by the engine mutex — the same boundary a
//! local caller's `&mut self` provides). `u8` ids (`< 0xFF`) are the
//! compact lane; `0xFF` escapes to `[0xFF][u16]` so growth never
//! requires a migration.
//!
//! The cache mirrors both directions in memory. Consistency rests on
//! the single-writer discipline; if several processes share one engine,
//! the cache degrades to connection lifetime (reload on reconnect) —
//! acceptable for a vocabulary that only grows.

use crate::model::index::{DICT_ID_SLOT, DICT_NAME_SLOT};
use crate::engine::storage::{KvBatch, VirtualStorage};
use std::collections::HashMap;

/// One dictionary cache, bound to one ns segment. Both directions plus
/// the allocation cursor; lazily loaded from the engine on first touch.
#[derive(Debug, Default)]
pub struct DictCache {
    by_id: HashMap<u16, String>,
    by_name: HashMap<String, u16>,
    next_id: u16,
    loaded: bool,
}

impl DictCache {
    /// Look up (or allocate) the id for `name` against the engine.
    /// `header` = the obj's key header (`[part][ns]`) — dictionary keys
    /// live inside the same ns segment as the data they name.
    pub fn id_for<S: VirtualStorage>(
        &mut self,
        store: &mut S,
        header: &[u8],
        name: &str,
    ) -> u16 {
        self.load(store, header);
        if let Some(id) = self.by_name.get(name) {
            return *id;
        }
        let id = self.next_id;
        self.next_id += 1;
        // One engine batch writes both directions: the dictionary is
        // consistent even if the caller's own write fails later (extra
        // dictionary entries are harmless — names are append-only).
        let mut batch = store.batch();
        let mut id_key = header.to_vec();
        id_key.extend_from_slice(&DICT_ID_SLOT.to_be_bytes());
        id_key.extend_from_slice(&id.to_be_bytes());
        batch.put(id_key, name.as_bytes().to_vec());
        let mut name_key = header.to_vec();
        name_key.extend_from_slice(&DICT_NAME_SLOT.to_be_bytes());
        name_key.extend_from_slice(name.as_bytes());
        batch.put(name_key, id.to_be_bytes().to_vec());
        let _ = store.commit_batch(batch);
        self.by_id.insert(id, name.to_owned());
        self.by_name.insert(name.to_owned(), id);
        id
    }

    /// Resolve ids from decoded frames to names. Unknown ids (a writer
    /// added entries this cache has not loaded — impossible under
    /// single-writer, possible with shared engines) fall back to a
    /// direct engine probe, then to `None` (caller renders a placeholder
    /// or drops the field).
    pub fn name_for<S: VirtualStorage>(
        &mut self,
        store: &S,
        header: &[u8],
        id: u16,
    ) -> Option<String> {
        self.load(store, header);
        if let Some(n) = self.by_id.get(&id) {
            return Some(n.clone());
        }
        // Direct probe before giving up (shared-engine catch-up).
        let mut key = header.to_vec();
        key.extend_from_slice(&DICT_ID_SLOT.to_be_bytes());
        key.extend_from_slice(&id.to_be_bytes());
        let name = store.get(&key)?;
        let name = String::from_utf8(name).ok()?;
        self.by_id.insert(id, name.clone());
        self.by_name.insert(name.clone(), id);
        if id >= self.next_id {
            self.next_id = id + 1;
        }
        Some(name)
    }

    /// Non-allocating name → id probe: does the cache already know this
    /// name (loaded or previously written through this handle)? No
    /// engine fallback — a name this handle never wrote and the load
    /// missed is treated as unknown (typed scans return empty rather
    /// than touching the store).
    pub fn by_name_get(&self, name: &str) -> Option<u16> {
        self.by_name.get(name).copied()
    }

    /// Load both directions from the engine once. Full-table load, not
    /// per-id probes: the vocabulary is small and append-only, so one
    /// scan_prefix covers every future read until this process writes a
    /// new name itself.
    fn load<S: VirtualStorage>(&mut self, store: &S, header: &[u8]) {
        if self.loaded {
            return;
        }
        // slot 2: [header][2][id u16 BE] → name
        let mut p2 = header.to_vec();
        p2.extend_from_slice(&DICT_ID_SLOT.to_be_bytes());
        for (suffix, name) in store.scan_suffix_kv(&p2) {
            if suffix.len() != 2 {
                continue;
            }
            let id = u16::from_be_bytes([suffix[0], suffix[1]]);
            if let Ok(n) = String::from_utf8(name) {
                self.by_id.insert(id, n);
            }
        }
        // slot 3 mirrors; also derives next_id (max + 1).
        let mut p3 = header.to_vec();
        p3.extend_from_slice(&DICT_NAME_SLOT.to_be_bytes());
        for (suffix, idv) in store.scan_suffix_kv(&p3) {
            if let Ok(n) = String::from_utf8(suffix) {
                if idv.len() == 2 {
                    let id = u16::from_be_bytes([idv[0], idv[1]]);
                    self.by_name.insert(n, id);
                    if id >= self.next_id {
                        self.next_id = id + 1;
                    }
                }
            }
        }
        self.loaded = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::storage::{KvBatch, VirtualStorage};
    use crate::engine::test_engine::TestStore;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Engine(Arc<Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>>);

    impl VirtualStorage for Engine {
        fn put(&self, key: Vec<u8>, value: Vec<u8>) {
            self.0.lock().unwrap().insert(key, value);
        }
        fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
            self.0.lock().unwrap().get(key).cloned()
        }
        fn del(&self, key: &[u8]) {
            self.0.lock().unwrap().remove(key);
        }
        fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
            self.0
                .lock()
                .unwrap()
                .range(prefix.to_vec()..)
                .take_while(|(k, _)| k.starts_with(prefix))
                .map(|(k, _)| k[prefix.len()..].to_vec())
                .collect()
        }
        fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
            let map = self.0.lock().unwrap();
            map.range(begin.to_vec()..)
                .take_while(|(k, _)| match end {
                    Some(end) => k.as_slice() < end,
                    None => true,
                })
                .map(|(k, _)| k.clone())
                .collect()
        }
    }

    impl crate::SharedVirtualStorage for Engine {
        fn shared_handle(&self) -> Self {
            self.clone()
        }
    }

    const HEADER: &[u8] = &[0x00, 0x21]; // ns 33, no partition

    #[test]
    fn allocates_sequentially_and_is_cached() {
        let mut store = TestStore::slatedb_mem();
        let mut d = DictCache::default();
        let a = d.id_for(&mut store, HEADER, "alpha");
        let b = d.id_for(&mut store, HEADER, "beta");
        let a2 = d.id_for(&mut store, HEADER, "alpha");
        assert_eq!((a, b, a2), (0, 1, 0), "first-seen claims next, repeat hits cache");

        // Both directions landed in the engine.
        let mut k2 = HEADER.to_vec();
        k2.extend_from_slice(&DICT_ID_SLOT.to_be_bytes());
        k2.extend_from_slice(&0u16.to_be_bytes());
        assert_eq!(store.get(&k2).unwrap(), b"alpha");
        let mut k3 = HEADER.to_vec();
        k3.extend_from_slice(&DICT_NAME_SLOT.to_be_bytes());
        k3.extend_from_slice(b"beta");
        assert_eq!(store.get(&k3).unwrap(), &1u16.to_be_bytes());
    }

    #[test]
    fn fresh_cache_loads_from_engine_and_resumes_numbering() {
        let mut store = TestStore::slatedb_mem();
        {
            let mut d = DictCache::default();
            d.id_for(&mut store, HEADER, "one");
            d.id_for(&mut store, HEADER, "two");
        }
        // A brand-new cache (process restart) resumes after the max.
        let mut d2 = DictCache::default();
        assert_eq!(d2.name_for(&store, HEADER, 0).unwrap(), "one");
        assert_eq!(d2.name_for(&store, HEADER, 1).unwrap(), "two");
        let fresh = d2.id_for(&mut store, HEADER, "three");
        assert_eq!(fresh, 2, "numbering resumes after the loaded max");
    }

    #[test]
    fn compact_ids_use_one_byte_and_escape_beyond() {
        let mut store = TestStore::slatedb_mem();
        let mut d = DictCache::default();
        let small = d.id_for(&mut store, HEADER, "small");
        assert!(small < 0xFF);
        // Directly seed an escaped-range name, then read it back.
        let mut batch = store.batch();
        let mut k2 = HEADER.to_vec();
        k2.extend_from_slice(&DICT_ID_SLOT.to_be_bytes());
        k2.extend_from_slice(&0x1234u16.to_be_bytes());
        batch.put(k2, b"big".to_vec());
        let mut k3 = HEADER.to_vec();
        k3.extend_from_slice(&DICT_NAME_SLOT.to_be_bytes());
        k3.extend_from_slice(b"big");
        batch.put(k3, 0x1234u16.to_be_bytes().to_vec());
        let _ = store.commit_batch(batch);
        assert_eq!(d.name_for(&store, HEADER, 0x1234).unwrap(), "big");
    }
}
