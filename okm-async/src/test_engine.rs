//! Test engine matrix: the engines core owns, plus the one that needs a runtime.
//!
//! `TestStore` is a concrete enum over the backends, so logic tests run against
//! actual engines — no mock, no parallel implementation to drift. This crate adds
//! exactly one arm to the matrix `okm_core::testing` defines: slatedb, whose
//! backend is async and therefore belongs to a crate that may host a runtime.
//!
//! Matrix: the ephemeral engine (ADR-0072 — in memory, zero fs, always
//! present, the default), slatedb in-memory, fjall temp-dir, redb temp-file.
//! The DEFAULT is okm's own non-persistent mode, so a test's cheap arm and a
//! temporary deployment's engine are the same engine; slatedb stays as the arm
//! that disagrees (ADR-0073). A test calls `TestStore::matrix()` (or the `for_each_engine!`
//! helper) and its body runs once per engine. The ephemeral arm is what holds
//! the engine that a temporary realm runs on to the same contract as the ones
//! that write to disk.
//!
//! Only the engine arms are gated: tests compile against whatever engines the
//! build has, and the ephemeral arm is always one of them.

use okm_core::engine::storage::{SharedVirtualStorage, VirtualStorage};

/// A concrete engine handle. Clones share the underlying keyspace (same
/// semantics as real engine handles).
#[derive(Clone)]
pub enum TestStore {
    /// One of the engines core owns — its own matrix, not a copy of it.
    Core(okm_core::testing::TestStore),
    /// slatedb over the in-memory object store. Zero fs.
    #[cfg(feature = "slatedb")]
    Slatedb(std::sync::Arc<crate::slatedb_backend::SlatedbSync>),
}

impl TestStore {
    /// All engines this build carries. Empty only when neither backend
    /// feature is enabled (not a supported configuration for tests).
    // every push here is feature-gated — clippy's vec_init_then_push
    // shape ("create then push immediately") is exactly what the
    // gated build legitimately is
    #[allow(unused_mut, clippy::vec_init_then_push)]
    pub fn matrix() -> Vec<(&'static str, Self)> {
        let mut out: Vec<(&'static str, Self)> = Vec::new();
        out.push(("ephemeral", Self::ephemeral()));
        #[cfg(feature = "slatedb")]
        out.push(("slatedb-mem", Self::slatedb_mem()));
        #[cfg(feature = "fjall")]
        out.push(("fjall", Self::fjall_tmp()));
        #[cfg(feature = "redb")]
        out.push(("redb", Self::redb_tmp()));
        out
    }

    pub fn ephemeral() -> Self {
        Self::Core(okm_core::testing::TestStore::ephemeral())
    }

    #[cfg(feature = "slatedb")]
    // Test-double construction: a machine that cannot open an in-memory slatedb
    // has nothing to test against.
    #[allow(
        clippy::expect_used,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )]
    pub fn slatedb_mem() -> Self {
        Self::Slatedb(std::sync::Arc::new(
            crate::slatedb_backend::SlatedbSync::open_mem("okm-test").expect("slatedb mem open"),
        ))
    }

    #[cfg(feature = "fjall")]
    pub fn fjall_tmp() -> Self {
        Self::Core(okm_core::testing::TestStore::fjall_tmp())
    }

    #[cfg(feature = "redb")]
    pub fn redb_tmp() -> Self {
        Self::Core(okm_core::testing::TestStore::redb_tmp())
    }

    /// Both variants are Arc-kernel handles: clone shares the keyspace.
    pub fn shared_handle(&self) -> Self {
        self.clone()
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Core(store) => store.name(),
            #[cfg(feature = "slatedb")]
            Self::Slatedb(_) => "slatedb-mem",
        }
    }
}

impl Default for TestStore {
    /// Default test engine: the ephemeral engine — no filesystem at all, and okm's own
    /// non-persistent mode (ADR-0072). A test's cheap arm and a temporary deployment's
    /// engine being the same engine is the point (ADR-0073): slatedb stays in
    /// [`matrix`](Self::matrix) as the arm that disagrees, not as the one everything
    /// starts on.
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
    fn note_table(&self, plan: &'static okm_core::model::table_plan::TablePlan) {
        match self {
            Self::Core(store) => store.note_table(plan),
            #[cfg(feature = "slatedb")]
            Self::Slatedb(_) => {}
        }
    }

    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> okm_core::error::Result<()> {
        match self {
            Self::Core(store) => store.put(key, value),
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.put_sync(key, value),
        }
    }
    fn get(&self, key: &[u8]) -> okm_core::error::Result<Option<Vec<u8>>> {
        match self {
            Self::Core(store) => store.get(key),
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.get_sync(key),
        }
    }
    fn del(&self, key: &[u8]) -> okm_core::error::Result<()> {
        match self {
            Self::Core(store) => store.del(key),
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.del_sync(key),
        }
    }
    fn scan_suffix(&self, prefix: &[u8]) -> okm_core::error::Result<Vec<Vec<u8>>> {
        match self {
            Self::Core(store) => store.scan_suffix(prefix),
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.scan_suffix_sync(prefix),
        }
    }
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> okm_core::engine::storage::ScanIter {
        match self {
            Self::Core(store) => store.scan_range(begin, end),
            #[cfg(feature = "slatedb")]
            Self::Slatedb(s) => s.scan_range_iter_sync(begin, end),
        }
    }
}
