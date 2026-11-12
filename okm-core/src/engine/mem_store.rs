//! The in-memory engine: a `BTreeMap` behind a mutex. One implementation,
//! two callers — a host's memory mode (aura's `Engine::Memory` binds it
//! directly, so no host grows its own stand-in) and this crate's own unit
//! tests.
//!
//! The test side exists because of a cargo fact, not a design preference.
//! `okm-core`'s unit tests would otherwise want `okm_async::TestStore` (the
//! matrix over real engines), but a dev-dependency that leads back to this
//! package makes cargo build `okm-core` TWICE — once for the tests, once for
//! the dependency edge — and the two copies' traits are distinct types, so
//! `TestStore` can never satisfy `impl VirtualStorage` here. Integration tests
//! (which link the lib as a dependency) do not have that problem and use the
//! real matrix.
//!
//! What this buys: ordered keys and the full `VirtualStorage`/`SharedVirtual`
//! semantics in memory, with no third-party engine. What it does not buy:
//! engine behaviour — ordered iteration quirks, the async adapter's threads and
//! channels, `fjall`'s snapshot semantics. Those are covered where the engines
//! live (`okm-async`'s tests for the async adapter, the integration matrix for
//! the sync ones).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::engine::storage::{ScanIter, SharedVirtualStorage, VirtualStorage, prefix_end};

/// Clone shares the keyspace (the same handle semantics as a real engine).
#[derive(Clone, Default)]
pub struct MemStore {
    map: Arc<Mutex<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<Vec<u8>, Vec<u8>>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl VirtualStorage for MemStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()> {
        self.map().insert(key, value);
        Ok(())
    }

    fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>> {
        Ok(self.map().get(key).cloned())
    }

    fn del(&self, key: &[u8]) -> crate::error::Result<()> {
        self.map().remove(key);
        Ok(())
    }

    fn scan_suffix(&self, prefix: &[u8]) -> crate::error::Result<Vec<Vec<u8>>> {
        let map = self.map();
        Ok(map
            .keys()
            .filter_map(|k| k.strip_prefix(prefix).map(<[u8]>::to_vec))
            .collect())
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        // Buffered by construction: this is a unit-test map, not an engine
        // with a cursor. `ScanIter::Buffered` is how a non-streaming backend
        // states itself (the trait's own shape), and the caller's `.collect()`
        // puts the eager list one call away.
        let _ = prefix_end; // the default `scan_suffix` bound is the same helper
        let map = self.map();
        let end = end.map(<[u8]>::to_vec);
        let pairs: Vec<crate::error::Result<(Vec<u8>, Vec<u8>)>> = map
            .range(begin.to_vec()..)
            .filter(|(k, _)| end.as_deref().is_none_or(|e| k.as_slice() < e))
            .map(|(k, v)| Ok((k.clone(), v.clone())))
            .collect();
        ScanIter::Buffered(pairs.into_iter())
    }
}

impl SharedVirtualStorage for MemStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}
