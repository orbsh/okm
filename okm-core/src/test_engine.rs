//! Test engine matrix: `TestStore` is a concrete enum over the real
//! backends, so logic tests run against actual engines — no mock, no
//! parallel implementation to drift.
//!
//! Matrix: slatedb in-memory (zero fs, default), fjall temp-dir, redb
//! temp-file. A test calls `TestStore::matrix()`
//! (or the `for_each_engine!` helper) and its body runs once per engine.
//!
//! Not gated behind a feature: tests compile against whatever engines the
//! build has; the matrix is simply the engines available.

use crate::storage::VirtualStorage;

/// A concrete engine handle. Clones share the underlying keyspace (same
/// semantics as real engine handles).
#[derive(Clone)]
pub enum TestStore {
    /// slatedb over the in-memory object store. Zero fs.
    #[cfg(feature = "slatedb")]
    Slatedb(std::sync::Arc<crate::slatedb_backend::SlatedbSync>),
    /// fjall over a temp dir. The dir lives as long as the handle.
    #[cfg(feature = "fjall")]
    Fjall {
        store: crate::fjall_backend::FjallStore,
        _dir: std::sync::Arc<tempfile::TempDir>,
    },
    /// redb over a temp file. The file lives as long as the handle.
    #[cfg(feature = "redb")]
    Redb {
        store: crate::redb_backend::RedbStore,
        _dir: std::sync::Arc<tempfile::TempDir>,
    },
}

impl TestStore {
    /// All engines this build carries. Empty only when neither backend
    /// feature is enabled (not a supported configuration for tests).
    pub fn matrix() -> Vec<( &'static str, Self )> {
        #[allow(unused_mut)] // variants are feature-gated; mut is needed when any engine is on
        let mut out = Vec::new();
        #[cfg(feature = "slatedb")]
        out.push(("slatedb-mem", Self::slatedb_mem()));
        #[cfg(feature = "fjall")]
        out.push(("fjall", Self::fjall_tmp()));
        #[cfg(feature = "redb")]
        out.push(("redb", Self::redb_tmp()));
        out
    }

    #[cfg(feature = "slatedb")]
    pub fn slatedb_mem() -> Self {
        Self::Slatedb(std::sync::Arc::new(
            crate::slatedb_backend::SlatedbSync::open_mem("okm-test")
                .expect("slatedb mem open"),
        ))
    }

    #[cfg(feature = "fjall")]
    pub fn fjall_tmp() -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store = crate::fjall_backend::FjallStore::open(dir.path(), "okm-test")
            .expect("fjall open");
        Self::Fjall { store, _dir: std::sync::Arc::new(dir) }
    }

    #[cfg(feature = "redb")]
    pub fn redb_tmp() -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store = crate::redb_backend::RedbStore::open(&dir.path().join("okm.redb"))
            .expect("redb open");
        Self::Redb { store, _dir: std::sync::Arc::new(dir) }
    }

    /// Both variants are Arc-kernel handles: clone shares the keyspace.
    pub fn shared_handle(&self) -> Self {
        self.clone()
    }

    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "slatedb")]
            Self::Slatedb(_) => "slatedb-mem",
            #[cfg(feature = "fjall")]
            Self::Fjall { .. } => "fjall",
            #[cfg(feature = "redb")]
            Self::Redb { .. } => "redb",
            #[cfg(not(any(feature = "slatedb", feature = "fjall", feature = "redb")))]
            _ => unreachable!("no engines enabled"),
        }
    }
}

#[cfg(feature = "slatedb")]
impl Default for TestStore {
    /// Default test engine: slatedb in-memory (zero fs, fastest to spin).
    fn default() -> Self {
        Self::slatedb_mem()
    }
}

impl crate::storage::SharedVirtualStorage for TestStore {
    fn shared_handle(&self) -> Self {
        self.shared_handle()
    }
}

#[cfg_attr(
    not(any(feature = "slatedb", feature = "fjall", feature = "redb")),
    allow(unused_variables)
)]
impl VirtualStorage for TestStore {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        match self {
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.put_sync(key, value),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.clone().put(key, value),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.clone().put(key, value),
            #[cfg(not(any(feature = "slatedb", feature = "fjall", feature = "redb")))]
            _ => unreachable!("TestStore: no engine features enabled"),
        }
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self {
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.get_sync(key),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.get(key),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.get(key),
            #[cfg(not(any(feature = "slatedb", feature = "fjall", feature = "redb")))]
            _ => unreachable!("TestStore: no engine features enabled"),
        }
    }
    fn del(&mut self, key: &[u8]) {
        match self {
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.del_sync(key),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.clone().del(key),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.clone().del(key),
            #[cfg(not(any(feature = "slatedb", feature = "fjall", feature = "redb")))]
            _ => unreachable!("TestStore: no engine features enabled"),
        }
    }
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        match self {
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.scan_suffix_sync(prefix),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.scan_suffix(prefix),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.scan_suffix(prefix),
            #[cfg(not(any(feature = "slatedb", feature = "fjall", feature = "redb")))]
            _ => unreachable!("TestStore: no engine features enabled"),
        }
    }
}
