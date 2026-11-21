//! Test engine matrix over the engines this crate owns.
//!
//! `TestStore` is a concrete enum rather than a mock, so a logic test runs against
//! actual engines and there is no parallel implementation to drift.
//!
//! The matrix is the ephemeral engine (ADR-0072 — in memory, zero fs, always
//! present, the default), fjall over a temp dir, and redb over a temp file. The
//! default is okm's own non-persistent mode, so a test's cheap arm and a temporary
//! deployment's engine are the same engine.
//!
//! This type lives here, not above: it is the lower crate's own test support, and
//! a crate cannot be tested through one of its dependents. The arm that is not here
//! is slatedb — its backend is async and needs a runtime, so it belongs to a crate
//! that may host one (`okm-async` wraps this enum and adds the arm). Core's tests
//! that once named slatedb by hand run on the default arm.
//!
//! Only the backend arms are gated: a test compiles against whatever engines the
//! build carries, and the ephemeral arm is always one of them.

use crate::engine::storage::{SharedVirtualStorage, VirtualStorage};

/// A concrete engine handle. Clones share the underlying keyspace (same semantics
/// as real engine handles).
#[derive(Clone)]
pub enum TestStore {
    /// The ephemeral engine (ADR-0072): a packed B+tree over chunked tables.
    /// Always in the matrix — it is a supported engine, not a test double.
    Ephemeral(crate::engine::ephemeral::EphemeralStore),
    /// fjall over a temp dir. The dir lives as long as the handle.
    #[cfg(feature = "fjall")]
    Fjall {
        store: crate::engine::fjall_backend::FjallStore,
        _dir: std::sync::Arc<tempfile::TempDir>,
    },
    /// redb over a temp file. The file lives as long as the handle.
    #[cfg(feature = "redb")]
    Redb {
        store: crate::engine::redb_backend::RedbStore,
        _dir: std::sync::Arc<tempfile::TempDir>,
    },
}

impl TestStore {
    /// All engines this build carries. Empty only when neither backend feature is
    /// enabled — not a supported configuration for tests, because the ephemeral
    /// arm is unconditional.
    // every push here is feature-gated — clippy's vec_init_then_push shape
    // ("create then push immediately") is exactly what the gated build legitimately is
    #[allow(unused_mut, clippy::vec_init_then_push)]
    pub fn matrix() -> Vec<(&'static str, Self)> {
        let mut out: Vec<(&'static str, Self)> = Vec::new();
        out.push(("ephemeral", Self::ephemeral()));
        #[cfg(feature = "fjall")]
        out.push(("fjall", Self::fjall_tmp()));
        #[cfg(feature = "redb")]
        out.push(("redb", Self::redb_tmp()));
        out
    }

    pub fn ephemeral() -> Self {
        Self::Ephemeral(crate::engine::ephemeral::EphemeralStore::new())
    }

    /// A temp-dir engine. A machine that cannot open one has nothing to test
    /// against, so the constructor is the failure point.
    #[cfg(feature = "fjall")]
    #[allow(
        clippy::expect_used,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )]
    pub fn fjall_tmp() -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store = crate::engine::fjall_backend::FjallStore::open(dir.path(), "okm-test")
            .expect("fjall open");
        Self::Fjall {
            store,
            _dir: std::sync::Arc::new(dir),
        }
    }

    /// A temp-file engine. Same failure story as [`fjall_tmp`](Self::fjall_tmp).
    #[cfg(feature = "redb")]
    #[allow(
        clippy::expect_used,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )]
    pub fn redb_tmp() -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store = crate::engine::redb_backend::RedbStore::open(&dir.path().join("okm.redb"))
            .expect("redb open");
        Self::Redb {
            store,
            _dir: std::sync::Arc::new(dir),
        }
    }

    /// Engine handles are Arc-kernel handles: clone shares the keyspace.
    pub fn shared_handle(&self) -> Self {
        self.clone()
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Ephemeral(_) => "ephemeral",
            #[cfg(feature = "fjall")]
            Self::Fjall { .. } => "fjall",
            #[cfg(feature = "redb")]
            Self::Redb { .. } => "redb",
        }
    }
}

impl Default for TestStore {
    /// Default test engine: the ephemeral engine — no filesystem at all, and okm's
    /// own non-persistent mode (ADR-0072). A test's cheap arm and a temporary
    /// deployment's engine being the same engine is the point (ADR-0073).
    fn default() -> Self {
        Self::ephemeral()
    }
}

impl SharedVirtualStorage for TestStore {
    fn shared_handle(&self) -> Self {
        self.shared_handle()
    }
}

impl VirtualStorage for TestStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()> {
        match self {
            Self::Ephemeral(store) => store.put(key, value),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.clone().put(key, value),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.clone().put(key, value),
        }
    }

    fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>> {
        match self {
            Self::Ephemeral(store) => store.get(key),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.get(key),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.get(key),
        }
    }

    fn del(&self, key: &[u8]) -> crate::error::Result<()> {
        match self {
            Self::Ephemeral(store) => store.del(key),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.clone().del(key),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.clone().del(key),
        }
    }

    fn scan_suffix(&self, prefix: &[u8]) -> crate::error::Result<Vec<Vec<u8>>> {
        match self {
            Self::Ephemeral(store) => store.scan_suffix(prefix),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.scan_suffix(prefix),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.scan_suffix(prefix),
        }
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> crate::engine::storage::ScanIter {
        match self {
            Self::Ephemeral(store) => store.scan_range(begin, end),
            #[cfg(feature = "fjall")]
            Self::Fjall { store, .. } => store.scan_range(begin, end),
            #[cfg(feature = "redb")]
            Self::Redb { store, .. } => store.scan_range(begin, end),
        }
    }
}
