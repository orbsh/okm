//! slatedb engine adapter: `SlatedbStore` (async engine over object storage).
//!
//! slatedb is an async API with immutable borrows (WAL/flush managed
//! internally), so this module provides the `KvEngineAsync` trait and
//! `AsyncCollection` - parallel to the sync `KvEngine`/`Collection` with the
//! same interface shape. Object stores are constructed via the
//! `slatedb::object_store` re-export so versions always match slatedb's
//! internals.

use crate::edge::KvEdge;
use crate::key::KeyEncode;
use slatedb::Db;
use slatedb::object_store::ObjectStore;
use std::ops::RangeFull;
use std::sync::Arc;

/// 异步引擎最小接口（与同步 KvEngine 对齐）
pub trait KvEngineAsync {
    async fn put(&self, key: Vec<u8>);
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

impl KvEngineAsync for SlatedbStore {
    async fn put(&self, key: Vec<u8>) {
        self.db
            .put(key, [])
            .await
            .expect("slatedb put failed");
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

/// 异步组装点：引擎 + 边类型 = 一条关系的操作面（平行于同步 Collection）
pub struct AsyncCollection<S, E> {
    pub store: S,
    _pd: std::marker::PhantomData<E>,
}

impl<S: KvEngineAsync, E: KvEdge> AsyncCollection<S, E> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            _pd: std::marker::PhantomData,
        }
    }

    /// 原子双写：正向 + 反向（slatedb 单 put 原子；双写崩溃窗口由上层 reconcile）
    pub async fn link(&self, a: &E::A, b: &E::B) {
        let e = E::from_parts(a.clone(), b.clone());
        self.store.put(e.forward_key()).await;
        self.store.put(e.reverse_key()).await;
    }

    pub async fn unlink(&self, a: &E::A, b: &E::B) {
        let e = E::from_parts(a.clone(), b.clone());
        self.store.del(&e.forward_key()).await;
        self.store.del(&e.reverse_key()).await;
    }

    /// A → Bs：正向扫描。要求 B 全量身份（可 decode）。
    pub async fn forward(&self, a: &E::A) -> Vec<E::B> {
        assert!(
            E::B_HEAD.is_empty(),
            "forward 需要 B 全量身份才能 decode 回类型"
        );
        let mut p = Vec::with_capacity(2 + E::a_head_width());
        p.extend_from_slice(&crate::edge::head_bytes(E::NS, false));
        E::encode_a_head(&mut p, a);
        self.store
            .scan_suffix(&p)
            .await
            .iter()
            .map(|sfx| E::B::decode(sfx))
            .collect()
    }

    /// B → As 的原始前缀字节（A 为截断身份时无法 decode）
    pub async fn reverse_raw(&self, b: &E::B) -> Vec<Vec<u8>> {
        let mut p = Vec::with_capacity(2 + E::b_head_width());
        p.extend_from_slice(&crate::edge::head_bytes(E::NS, true));
        E::encode_b_head(&mut p, b);
        self.store.scan_suffix(&p).await
    }
}

// object_store 便捷 re-export：调用方构造 InMemory/S3 store 用 slatedb 的版本，避免版本分裂
pub use slatedb::object_store;
