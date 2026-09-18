//! slatedb engine adapter: `SlatedbStore` (async engine over object storage).
//!
//! slatedb is an async API with immutable borrows (WAL/flush managed
//! internally), so this module provides the `VirtualStorageAsync` trait and
//! `AsyncJunction` - parallel to the sync `VirtualStorage`/`Edge` with the
//! same interface shape. Object stores are constructed via the
//! `slatedb::object_store` re-export so versions always match slatedb's
//! internals.

use crate::model::junction::KvJunction;
use crate::model::key::KeyEncode;
use crate::engine::storage::VirtualStorage;
use slatedb::Db;
use slatedb::object_store::ObjectStore;
use std::ops::RangeFull;
use std::sync::Arc;

/// Sync facade over a slatedb instance: an internal current-thread runtime
/// drives the async engine. This is what the sync `VirtualStorage` tests
/// and engines run on — the in-memory object store gives a zero-fs
/// test engine; a real object store gives production.
///
/// One runtime per store handle; handlers must not call across handles in
/// nested fashion (re-entrant block_on panics). OKM's call shapes are flat
/// (engine methods only), so this holds.
pub struct SlatedbSync {
    rt: tokio::runtime::Runtime,
    db: Db,
}

impl SlatedbSync {
    /// In-memory instance: zero fs, per-test isolation by construction
    /// (each call builds a fresh InMemory object store).
    pub fn open_mem(name: &str) -> Result<Self, slatedb::Error> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio current-thread runtime");
        let store: Arc<dyn ObjectStore> = Arc::new(slatedb::object_store::memory::InMemory::new());
        let db = rt.block_on(slatedb::Db::open(format!("/{name}"), store))?;
        Ok(Self { rt, db })
    }

    /// Sync adapter over an already-open async Db.
    pub fn from_db(rt: tokio::runtime::Runtime, db: Db) -> Self {
        Self { rt, db }
    }

    /// Access the async handle (async call sites bypass block_on).
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Access the runtime (async call sites).
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.rt
    }
}

/// 异步引擎最小接口（与同步 VirtualStorage 对齐）
pub trait VirtualStorageAsync {
    async fn put(&self, key: Vec<u8>, value: Vec<u8>);
    async fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    async fn del(&self, key: &[u8]);
    /// 前缀扫描，返回每个 key 的"剩余段"（去掉 prefix）
    async fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>>;
}

/// slatedb 包装。ns 前缀由 key 编码自带；一个 Db 可承载所有边类型。
pub struct SlatedbStore {
    db: Db,
}

impl SlatedbStore {
    pub async fn open(path: &str, store: Arc<dyn ObjectStore>) -> Result<Self, slatedb::Error> {
        Ok(Self {
            db: Db::open(path, store).await?,
        })
    }

    pub fn from_db(db: Db) -> Self {
        Self { db }
    }
}

impl VirtualStorageAsync for SlatedbStore {
    async fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        self.db.put(key, value).await.expect("slatedb put failed");
    }
    async fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.db
            .get(key)
            .await
            .expect("slatedb get failed")
            .map(|v| v.to_vec())
    }
    async fn del(&self, key: &[u8]) {
        self.db.delete(key).await.expect("slatedb delete failed");
    }
    async fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut it = self
            .db
            .scan_prefix(prefix, RangeFull)
            .await
            .expect("slatedb scan failed");
        let mut out = Vec::new();
        while let Some(kv) = it.next().await.expect("slatedb iter failed") {
            out.push(kv.key[prefix.len()..].to_vec());
        }
        out
    }
}

/// 异步 junction 装配点：引擎 + junction 类型 = 一条关系的操作面（平行于同步 Junction）
pub struct AsyncJunction<S, E> {
    pub store: S,
    _pd: std::marker::PhantomData<E>,
}

type AKey<E> = <<E as KvJunction>::A as crate::model::index::Document>::Key;
type BKey<E> = <<E as KvJunction>::B as crate::model::index::Document>::Key;

impl<S: VirtualStorageAsync, E: KvJunction> AsyncJunction<S, E> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            _pd: std::marker::PhantomData,
        }
    }

    /// 原子双写：两端各一条单向 entry（slatedb 单 put 原子；崩溃窗口由上层 reconcile）
    pub async fn link(&self, a: &AKey<E>, b: &BKey<E>) {
        let e = E::from_parts(a.clone(), b.clone());
        self.store.put(e.a_side_key(), Vec::new()).await;
        self.store.put(e.b_side_key(), Vec::new()).await;
    }

    pub async fn unlink(&self, a: &AKey<E>, b: &BKey<E>) {
        let e = E::from_parts(a.clone(), b.clone());
        self.store.del(&e.a_side_key()).await;
        self.store.del(&e.b_side_key()).await;
    }

    /// A 端扫描：与 a 相连的全部 B 身份（要求 B 全量身份，可 decode）。
    pub async fn forward(&self, a: &AKey<E>) -> Vec<BKey<E>> {
        assert!(
            E::B_HEAD.is_empty(),
            "forward 需要 B 全量身份才能 decode 回类型"
        );
        let p = E::a_side_prefix(a);
        self.store
            .scan_suffix(&p)
            .await
            .iter()
            .map(|sfx| BKey::<E>::decode(sfx))
            .collect()
    }

    /// B 端扫描的原始前缀字节（A 为截断身份时无法 decode）
    pub async fn reverse_raw(&self, b: &BKey<E>) -> Vec<Vec<u8>> {
        let p = E::b_side_prefix(b);
        self.store.scan_suffix(&p).await
    }
}

// object_store 便捷 re-export：调用方构造 InMemory/S3 store 用 slatedb 的版本，避免版本分裂
pub use slatedb::object_store;

impl SlatedbSync {
    /// slatedb ops are &self-safe (engine handles concurrency internally);
    /// these inherent methods are what shared handles call.
    pub fn put_sync(&self, key: Vec<u8>, value: Vec<u8>) {
        self.rt.block_on(self.db.put(key, value)).expect("slatedb put failed");
    }
    pub fn get_sync(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.rt
            .block_on(self.db.get(key))
            .expect("slatedb get failed")
            .map(|v| v.to_vec())
    }
    pub fn del_sync(&self, key: &[u8]) {
        self.rt.block_on(self.db.delete(key)).expect("slatedb delete failed");
    }
    pub fn scan_suffix_sync(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut it = self
            .rt
            .block_on(self.db.scan_prefix(prefix, RangeFull))
            .expect("slatedb scan failed");
        let mut out = Vec::new();
        while let Some(kv) = self.rt.block_on(it.next()).expect("slatedb iter failed") {
            out.push(kv.key[prefix.len()..].to_vec());
        }
        out
    }
}

impl VirtualStorage for SlatedbSync {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.put_sync(key, value)
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_sync(key)
    }
    fn del(&mut self, key: &[u8]) {
        self.del_sync(key)
    }
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut it = self
            .rt
            .block_on(self.db.scan_prefix(prefix, RangeFull))
            .expect("slatedb scan failed");
        let mut out = Vec::new();
        while let Some(kv) = self.rt.block_on(it.next()).expect("slatedb iter failed") {
            out.push(kv.key[prefix.len()..].to_vec());
        }
        out
    }
}
