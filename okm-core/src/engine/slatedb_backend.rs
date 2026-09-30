//! slatedb engine adapter: `SlatedbStore` (async engine over object storage)
//! and `AsyncJunction` (its junction assembly point).
//!
//! slatedb is an async API with immutable borrows (WAL/flush managed
//! internally), so this module wraps it for the async world; the async
//! engine contract itself — `VirtualStorageAsync` — lives in
//! `engine::storage` (ADR-0028: the aligned surface is the engine
//! contract, not a slatedb property). Object stores are constructed via
//! the `slatedb::object_store` re-export so versions always match
//! slatedb's internals.

use crate::engine::storage::VirtualStorage;
// The trait + the pair-scan twin live in engine::storage (ADR-0028);
// re-exported here so the slatedb-era import path keeps resolving.
pub use crate::engine::storage::{VirtualStorageAsync, scan_suffix_kv_async};
use crate::model::junction::KvJunction;
use crate::model::key::KeyEncode;
use slatedb::Db;
use slatedb::object_store::ObjectStore;
use std::ops::RangeFull;
use std::sync::Arc;

/// Owned lazy adapter over the slatedb driver thread: the iterator
/// consumes a per-item channel the driver fills (one `block_on` batch
/// on the driver thread, zero `block_on` on the consumer thread — the
/// consumer may live INSIDE a tokio context; a runtime-driven block_on
/// there panics). Backwards walk (`next_back`) has no native
/// counterpart — it drains the remaining range once and reads from the
/// tail (laziness lost, semantics kept — the old shape's trade).
pub struct SlatedbIter {
    rx: ItemStream,
    done: bool,
    back_buf: Option<std::vec::IntoIter<(Vec<u8>, Vec<u8>)>>,
}

impl Iterator for SlatedbIter {
    type Item = (Vec<u8>, Vec<u8>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.rx.recv() {
            Ok(Some(kv)) => Some(kv),
            Ok(None) | Err(_) => {
                self.done = true;
                None
            }
        }
    }
}

impl DoubleEndedIterator for SlatedbIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.back_buf.is_none() {
            // Materialize everything not yet consumed into a reversed
            // tail buffer (the old `fill_back_buf` semantics) — a
            // BLOCKING drain to the stream's end sentinel (a try_iter
            // would race the driver and silently drop items it has
            // not sent yet).
            let mut tail: Vec<(Vec<u8>, Vec<u8>)> = self.rx.iter().flatten().collect();
            tail.reverse();
            self.back_buf = Some(tail.into_iter());
            self.done = true;
        }
        self.back_buf.as_mut().and_then(std::iter::Iterator::next)
    }
}

/// The per-item channel of a lazy scan stream (the driver produces
/// `Some((k, v))` items, `None` ends the range; a dropped consumer
/// stops the producer on its send error).
type ItemStream = std::sync::mpsc::Receiver<Option<(Vec<u8>, Vec<u8>)>>;

/// Commands the driver thread executes against the async Db. Every
/// arm is flat (no re-entrant nesting — okm's call shapes are engine
/// methods only, the same rule the old shared-runtime doc recorded).
enum Cmd {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Get {
        key: Vec<u8>,
        reply: std::sync::mpsc::Sender<Option<Vec<u8>>>,
    },
    Del {
        key: Vec<u8>,
    },
    /// Full keys in [begin, end) (the `scan_range_sync` contract);
    /// `end: None` = unbounded tail.
    ScanRange {
        begin: Vec<u8>,
        end: Option<Vec<u8>>,
        reply: std::sync::mpsc::Sender<Vec<Vec<u8>>>,
    },
    /// Keys MINUS the prefix under `prefix` (the `scan_suffix_sync`
    /// contract).
    ScanSuffix {
        prefix: Vec<u8>,
        reply: std::sync::mpsc::Sender<Vec<Vec<u8>>>,
    },
    /// Lazy forward stream: the driver produces `(key, value)` items
    /// until drained or the consumer drops the iterator (send fails).
    /// `end: None` = unbounded tail.
    ScanStream {
        begin: Vec<u8>,
        end: Option<Vec<u8>>,
        reply: std::sync::mpsc::Sender<ItemStream>,
    },
}

/// Sync facade over a slatedb instance: a DEDICATED driver thread owns
/// the current-thread runtime and the Db, and executes commands from a
/// channel. This is what the sync `VirtualStorage` tests and engines
/// run on — the in-memory object store gives a zero-fs test engine; a
/// real object store gives production.
///
/// Why a thread and not a held runtime: `Runtime::block_on` panics
/// when called from a thread that already has a tokio context entered
/// — aura's realm line constructs and drives injections inside
/// `spawn_blocking`, which KEEPS the context. The old shape (runtime
/// handle + block_on per call) worked only for consumers outside any
/// runtime, and that assumption silently broke the first time a real
/// host embedded the engine. Commands cross an mpsc instead: the
/// consumer blocks on the channel, never on `block_on`.
pub struct SlatedbSync {
    tx: Arc<std::sync::mpsc::Sender<Cmd>>,
}

fn drive(rt: tokio::runtime::Runtime, db: Db, rx: std::sync::mpsc::Receiver<Cmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Put { key, value } => {
                rt.block_on(db.put(key, value)).expect("slatedb put failed");
            }
            Cmd::Get { key, reply } => {
                let v = rt
                    .block_on(db.get(&key))
                    .expect("slatedb get failed")
                    .map(|v| v.to_vec());
                let _ = reply.send(v);
            }
            Cmd::Del { key } => {
                rt.block_on(db.delete(key)).expect("slatedb delete failed");
            }
            Cmd::ScanRange { begin, end, reply } => {
                let mut it = match end {
                    Some(end) => rt
                        .block_on(db.scan_prefix(b"", begin..end))
                        .expect("slatedb scan failed"),
                    None => rt
                        .block_on(db.scan_prefix(b"", begin..))
                        .expect("slatedb scan failed"),
                };
                let mut out = Vec::new();
                while let Some(kv) = rt.block_on(it.next()).expect("slatedb iter failed") {
                    out.push(kv.key.to_vec());
                }
                let _ = reply.send(out);
            }
            Cmd::ScanSuffix { prefix, reply } => {
                let mut it = rt
                    .block_on(db.scan_prefix(&prefix, RangeFull))
                    .expect("slatedb scan failed");
                let mut out = Vec::new();
                while let Some(kv) = rt.block_on(it.next()).expect("slatedb iter failed") {
                    out.push(kv.key[prefix.len()..].to_vec());
                }
                let _ = reply.send(out);
            }
            Cmd::ScanStream { begin, end, reply } => {
                let (item_tx, item_rx) = std::sync::mpsc::channel();
                // Hand the consumer its receiver FIRST, then produce.
                if reply.send(item_rx).is_err() {
                    continue;
                }
                let mut it = match end {
                    Some(end) => rt
                        .block_on(db.scan_prefix(b"", begin..end))
                        .expect("slatedb scan failed"),
                    None => rt
                        .block_on(db.scan_prefix(b"", begin..))
                        .expect("slatedb scan failed"),
                };
                loop {
                    match rt.block_on(it.next()) {
                        Ok(Some(kv)) => {
                            if item_tx
                                .send(Some((kv.key.to_vec(), kv.value.to_vec())))
                                .is_err()
                            {
                                break; // consumer dropped the iterator
                            }
                        }
                        Ok(None) => {
                            let _ = item_tx.send(None);
                            break;
                        }
                        Err(e) => panic!("slatedb iter failed: {e}"),
                    }
                }
            }
        }
    }
}

impl SlatedbSync {
    /// In-memory instance: zero fs, per-test isolation by construction
    /// (each call builds a fresh InMemory object store). The open runs
    /// on the driver thread — the caller may hold a tokio context, and
    /// `block_on` there panics.
    pub fn open_mem(name: &str) -> Result<Self, slatedb::Error> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let name = name.to_string();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio current-thread runtime");
            let store: Arc<dyn ObjectStore> =
                Arc::new(slatedb::object_store::memory::InMemory::new());
            match rt.block_on(slatedb::Db::open(format!("/{name}"), store)) {
                Ok(db) => {
                    let _ = ready_tx.send(Ok(()));
                    drive(rt, db, cmd_rx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        });
        ready_rx.recv().expect("slatedb open thread")?;
        Ok(Self { tx: Arc::new(cmd_tx) })
    }

    /// Sync adapter over an already-open async Db (the production
    /// shape: the caller built the Db on its own runtime; this hands
    /// the Db AND the runtime to a driver thread).
    pub fn from_db(rt: tokio::runtime::Runtime, db: Db) -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || drive(rt, db, cmd_rx));
        Self { tx: Arc::new(cmd_tx) }
    }

    fn request<R>(&self, make: impl FnOnce(std::sync::mpsc::Sender<R>) -> Cmd) -> R {
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx.send(make(tx)).expect("slatedb driver alive");
        rx.recv().expect("slatedb driver reply")
    }
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
    async fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        if let Some(end) = end
            && end <= begin {
                return Vec::new();
            }
        // The subrange is relative to the prefix (slatedb semantics);
        // an empty prefix makes the FULL-key range the subrange.
        let mut it = match end {
            Some(end) => self.db.scan_prefix(b"", begin..end).await,
            None => self.db.scan_prefix(b"", begin..).await,
        }
        .expect("slatedb scan failed");
        let mut out = Vec::new();
        while let Some(kv) = it.next().await.expect("slatedb iter failed") {
            out.push(kv.key.to_vec());
        }
        out
    }
    /// Override of the buffered default (ADR-0027): materialize the range
    /// in ONE native pass — keys and values come from the same
    /// `DbIterator`, fixing the default's N+1 (`scan_range` + per-key
    /// `get`). No lazy `SlatedbIter` wrapper here on purpose: its
    /// `next()` calls `block_on`, which PANICS inside the async caller's
    /// runtime (module header; the sync world's `SlatedbSync` is the only
    /// legal home for the lazy adapter).
    async fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> crate::engine::storage::ScanIter {
        if let Some(end) = end
            && end <= begin {
                return crate::engine::storage::ScanIter::Buffered(
                    Vec::new().into_iter(),
                );
            }
        let mut it = match end {
            Some(end) => self.db.scan_prefix(b"", begin..end).await,
            None => self.db.scan_prefix(b"", begin..).await,
        }
        .expect("slatedb scan failed");
        let mut pairs = Vec::new();
        while let Some(kv) = it.next().await.expect("slatedb iter failed") {
            pairs.push((kv.key.to_vec(), kv.value.to_vec()));
        }
        crate::engine::storage::ScanIter::Buffered(pairs.into_iter())
    }
    /// Native batch (ADR-0027): slatedb's `WriteBatch` ships as ONE
    /// `db.write` — a real single-write atomic form, not a replay; the
    /// durable wait mirrors the sync engines' WAL boundary.
    async fn commit_batch(&mut self, batch: crate::engine::storage::MemBatch) -> Result<(), String> {
        let mut wb = slatedb::WriteBatch::new();
        for (k, v) in &batch.ops {
            match v {
                Some(v) => wb.put(k.as_slice(), v.as_slice()),
                None => wb.delete(k.as_slice()),
            }
        }
        let h = self.db.write(wb).await.map_err(|e| e.to_string())?;
        h.await_durable().await.map_err(|e| e.to_string())
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
    /// slatedb ops are &self-safe (the driver thread serializes them);
    /// these inherent methods are what shared handles call — each is
    /// one command over the channel, never a `block_on` on the CALLER's
    /// thread (that is the panic the driver-thread shape exists to kill:
    /// aura's realm drives these inside spawn_blocking, a live tokio
    /// context).
    pub fn put_sync(&self, key: Vec<u8>, value: Vec<u8>) {
        self.tx
            .send(Cmd::Put { key, value })
            .expect("slatedb driver alive");
        // Fire-and-forget is safe by channel ORDER: one mpsc, one
        // driver — a later command (the Collection's read-back of its
        // own batch, or the next write) is processed after this Put.
        // Consumers that must observe a write against EXTERNAL writers
        // ride Get, whose reply round trip closes the gap.
    }
    pub fn get_sync(&self, key: &[u8]) -> Option<Vec<u8>> {
        let key = key.to_vec();
        self.request(|reply| Cmd::Get { key, reply })
    }
    pub fn del_sync(&self, key: &[u8]) {
        self.tx
            .send(Cmd::Del { key: key.to_vec() })
            .expect("slatedb driver alive");
    }
    pub fn scan_range_sync(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        if let Some(end) = end
            && end <= begin
        {
            return Vec::new();
        }
        let begin = begin.to_vec();
        let end = end.map(|e| e.to_vec());
        self.request(|reply| Cmd::ScanRange { begin, end, reply })
    }
    /// Owned lazy adapter over the driver's per-item channel (ADR-0027
    ///'s laziness contract: the driver walks the range as the consumer
    /// pulls; a dropped iterator stops the producer on its send error).
    pub fn scan_range_iter_sync(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> super::storage::ScanIter {
        if let Some(end) = end
            && end <= begin
        {
            let empty: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            return super::storage::ScanIter::Buffered(empty.into_iter());
        }
        let begin = begin.to_vec();
        let end = end.map(|e| e.to_vec());
        let rx = self.request(|reply| Cmd::ScanStream { begin, end, reply });
        super::storage::ScanIter::Slatedb(SlatedbIter { rx, done: false, back_buf: None })
    }

    pub fn scan_suffix_sync(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let prefix = prefix.to_vec();
        self.request(|reply| Cmd::ScanSuffix { prefix, reply })
    }
}

impl VirtualStorage for SlatedbSync {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        self.put_sync(key, value)
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_sync(key)
    }
    fn del(&self, key: &[u8]) {
        self.del_sync(key)
    }
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.scan_range_sync(prefix, None)
            .into_iter()
            .filter_map(|k| k.strip_prefix(prefix).map(|s| s.to_vec()))
            .collect()
    }
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        self.scan_range_sync(begin, end)
    }
    fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> super::storage::ScanIter {
        self.scan_range_iter_sync(begin, end)
    }
}
