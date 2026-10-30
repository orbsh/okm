//! Storage abstraction: the sync [`VirtualStorage`] and async
//! [`VirtualStorageAsync`] engine contracts (aligned surfaces,
//! ADR-0027/0028) plus the shared batch carrier. Every backend (fjall /
//! slatedb / redb / localStorage / wire client) implements one of them.
//! Tests run against real engines via [`crate::engine::test_engine::TestStore`].



/// Minimal KV engine interface: one ordered-read primitive (`scan_range`)
/// plus point reads and batches. The `fjall` / `slatedb` features each
/// provide an implementation; tests run the real engines through
/// [`crate::engine::test_engine::TestStore`].
/// The exclusive upper bound of a prefix as a FULL key: increment the
/// last byte with carry (`[0x01, 0x7F]` -> `[0x01, 0x80]`). `None` when
/// the prefix is all `0xFF` (no finite terminator — the range is
/// unbounded). This is how prefix semantics ride on top of a range
/// primitive without a second physical operation.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    for b in end.iter_mut().rev() {
        if *b < 0xFF {
            *b += 1;
            return Some(end);
        }
        *b = 0x00;
    }
    None
}

pub trait VirtualStorage {
    /// Point write. `Err` is the engine refusing the bytes — the mirror of
    /// `get`'s error, and the reason this answers `Result` instead of `()`:
    /// a write that cannot report a refused commit has no error channel at all,
    /// so every backend used to swallow the failure as a panic in its own text
    /// (ADR-0037's follow-up, the storage trait).
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()>;
    /// Point read. `Ok(None)` is "no such key" — the normal answer of a KV
    /// read, never an error. `Err` is the engine failing to answer at all
    /// (I/O, a fenced handle, a dead transport): the two used to be the same
    /// `None`, which made a read failure look like an absent row.
    fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>>;
    /// Point delete. `Ok(())` also covers "there was nothing to delete": absence
    /// is not a failure, the same rule `get` follows for `Ok(None)`. `Err` is the
    /// engine refusing the operation.
    fn del(&self, key: &[u8]) -> crate::error::Result<()>;
    /// Prefix scan; returns each matching key's "suffix" (prefix removed).
    /// A convenience bound over [`scan_range`](Self::scan_range): the
    /// physical operation is the same ordered iteration, the prefix is
    /// the range whose end is the prefix's terminator (see
    /// [`prefix_end`]). Engines override when their native prefix scan
    /// beats a range (`slatedb`'s `scan_prefix(prefix, subrange)`).
    fn scan_suffix(&self, prefix: &[u8]) -> crate::error::Result<Vec<Vec<u8>>> {
        Ok(self
            .scan_range(prefix, prefix_end(prefix).as_deref())?
            .into_iter()
            // The default MUST honor the documented return shape: `scan_range`
            // hands back FULL keys, so the prefix is stripped here. A default
            // that forwarded full keys kept every tail-slicing caller working
            // (they slice from the end) while silently turning every
            // LENGTH-based judgement into a no-op — an impl with only the raw
            // ops (a host-injected engine) then reported suffixes one prefix
            // too long, and exact matching could never fire there. The range
            // holds exactly the keys carrying `prefix`, so the slice is total;
            // `get` keeps it panic-free regardless.
            .map(|k| k.get(prefix.len()..).unwrap_or(&[]).to_vec())
            .collect())
    }
    /// Range scan over FULL keys: `[begin, end)` in byte order —
    /// `None` end = unbounded. Begin is inclusive. This is the ONE
    /// ordered-read primitive of the engine contract; prefix scanning
    /// is its special case (see `scan_suffix`). Byte order == value
    /// order for every OKM-encoded field (BE fixed-width, VarInt's
    /// prefix-monotonic encoding), so a `1 < a < 100` predicate IS a
    /// key interval — the engine reads only rows inside it.
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> crate::error::Result<Vec<Vec<u8>>>;

    /// LAZY range scan: full `(key, value)` pairs in byte order, pulled
    /// on demand — half-way abandonment (LIMIT, first-match) costs only
    /// the reads actually performed. The opaque [`ScanIter`] is the
    /// trait's streaming contract: every native backend iterator (fjall
    /// `Iter`, redb `OwnedRange`, slatedb `DbIterator`) is 'static and
    /// owned, so wrapping is free; buffers never materialize unless the
    /// consumer collects. `scan_range`'s default derives from here.
    /// Remote engines degrade to a buffered round trip (the wire frame
    /// already batches the answer).
    fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> ScanIter {
        // Sound-but-buffered default: engines with owned native
        // iterators (fjall / redb / slatedb) override this method; the
        // default materializes the whole range first (a borrow cannot
        // span the trait boundary) and hands back an owned iterator.
        // Errors ride the ITEMS: the constructor cannot fail on behalf of
        // an engine whose failure shows up per key.
        let mut pairs: Vec<crate::error::Result<(Vec<u8>, Vec<u8>)>> = Vec::new();
        match self.scan_range(begin, end) {
            Ok(keys) => {
                for k in keys {
                    match self.get(&k) {
                        Ok(Some(v)) => pairs.push(Ok((k, v))),
                        // The key vanished between the scan and the read: the
                        // range is not a snapshot, and a missing value is a
                        // normal race, not a failure.
                        Ok(None) => {}
                        Err(e) => {
                            pairs.push(Err(e));
                            break;
                        }
                    }
                }
            }
            Err(e) => pairs.push(Err(e)),
        }
        ScanIter::Buffered(pairs.into_iter())
    }
    /// Prefix scan returning `(key suffix, value)` pairs, in key order,
    /// is NOT a trait method — it lives as the free function
    /// [`scan_suffix_kv`] (ADR-0027).
    /// Commit an accumulated [`Batch`]: one engine-level write over
    /// all ops in the list. This is the cross-collection atomicity
    /// primitive (ADR-0003): single-collection writes are already atomic
    /// inside `Collection::put`; primary + index across two assembly
    /// points share one batch and commit once. Default replays the op
    /// list through put/del (single-threaded, so the sequence is atomic
    /// within the caller's write order); engines with a native batch API
    /// (fjall `Batch`, redb one write txn) override to execute the list
    /// as ONE engine-level atomic unit — the single WAL commit is real,
    /// not a replay.
    fn commit_batch(&self, batch: Batch) -> crate::error::Result<()> {
        for (k, v) in batch.ops {
            match v {
                Some(v) => self.put(k, v)?,
                None => self.del(&k)?,
            }
        }
        Ok(())
    }
}

/// Prefix scan returning `(key suffix, value)` pairs, in key order:
/// ONE pass over [`VirtualStorage::scan_range_iter`] (ADR-0027 moved this
/// off the trait — its old default derived from `scan_suffix` + `get`,
/// an N+1 shape no engine ever overrode).
pub fn scan_suffix_kv<S: VirtualStorage + ?Sized>(
    store: &S,
    prefix: &[u8],
) -> crate::error::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let end = prefix_end(prefix);
    let mut out = Vec::new();
    for item in store.scan_range_iter(prefix, end.as_deref()) {
        let (mut full, v) = item?;
        // Drop the prefix from the key the iterator already allocated instead of copying
        // the suffix into a second buffer: the range guarantees the key starts with the
        // prefix, so this is a memmove inside one allocation, not two allocations.
        full.drain(..prefix.len().min(full.len()));
        out.push((full, v));
    }
    Ok(out)
}

/// An engine that can be genuinely shared across hosts and handles:
/// the `shared_handle()` view IS the same physical keyspace (fjall /
/// slatedb store handles are Arc-kernel and already behave this way).
/// `NestStorage` requires it — a nest wraps the engine in an
/// `Arc<Mutex<S>>`, and "wrapped" must mean shared, not copied.
/// Deep-copy-Clone engines (test maps) deliberately do not implement
/// this: a copied engine behind two hosts would silently fork the
/// keyspace.
/// The engine contract's streaming iterator, returned by
/// [`VirtualStorage::scan_range_iter`]. An opaque enum over the
/// backends' native owned iterators rather than a `dyn` box: the enum
/// keeps `DoubleEndedIterator` — a consumer may walk the range
/// backwards (e.g. "last N entries") without a second buffer pass.
/// Variants without a native backwards walk (slatedb's async-forward
/// `DbIterator`, the remote buffered round trip) degrade to buffered
/// `next_back` over a materialized tail — semantics identical, laziness
/// lost only where the engine cannot give it back.
pub enum ScanIter {
    /// fjall `Iter` — native snapshot, both directions.
    #[cfg(feature = "fjall")]
    Fjall(fjall::Iter),
    /// redb `OwnedRange` — 'static via Arc'd txn guard, both directions.
    #[cfg(feature = "redb")]
    Redb(redb::OwnedRange<&'static [u8], &'static [u8]>),
    /// slatedb `DbIterator` — forward-only (async adapter); backwards
    /// walk buffers a materialized copy of the remaining range.
    #[cfg(feature = "slatedb")]
    Slatedb(super::slatedb_backend::SlatedbIter),
    /// Remote paged stream (ADR-0021): lazy over the wire, one
    /// OP_SCAN_STREAM page per refill; `next_back` degrades to buffered.
    Remote(super::nest::RemoteScanIter),
    /// Owned buffer: the trait default, and any test map.
    Buffered(std::vec::IntoIter<crate::error::Result<(Vec<u8>, Vec<u8>)>>),
}

impl Iterator for ScanIter {
    /// An error can surface mid-scan (a native iterator fault, a dropped
    /// transport on a remote page), so it rides the ITEM — the constructor
    /// cannot answer for it, and swallowing it is how "failed" used to read as
    /// "empty".
    type Item = crate::error::Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            #[cfg(feature = "fjall")]
            ScanIter::Fjall(it) => it.next().map(|g| match g.into_inner() {
                Ok((k, v)) => Ok((k.to_vec(), v.to_vec())),
                Err(e) => Err(crate::error::Error::Engine(e.to_string())),
            }),
            #[cfg(feature = "redb")]
            ScanIter::Redb(it) => it.next().map(|r| match r {
                Ok((k, v)) => Ok((k.value().to_vec(), v.value().to_vec())),
                Err(e) => Err(crate::error::Error::Engine(e.to_string())),
            }),
            #[cfg(feature = "slatedb")]
            ScanIter::Slatedb(it) => it.next(),
            ScanIter::Remote(it) => it.next(),
            ScanIter::Buffered(it) => it.next(),
        }
    }
}

impl DoubleEndedIterator for ScanIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            #[cfg(feature = "fjall")]
            ScanIter::Fjall(it) => it.next_back().map(|g| match g.into_inner() {
                Ok((k, v)) => Ok((k.to_vec(), v.to_vec())),
                Err(e) => Err(crate::error::Error::Engine(e.to_string())),
            }),
            #[cfg(feature = "redb")]
            ScanIter::Redb(it) => it.next_back().map(|r| match r {
                Ok((k, v)) => Ok((k.value().to_vec(), v.value().to_vec())),
                Err(e) => Err(crate::error::Error::Engine(e.to_string())),
            }),
            #[cfg(feature = "slatedb")]
            ScanIter::Slatedb(it) => it.next_back(),
            ScanIter::Remote(it) => it.next_back(),
            ScanIter::Buffered(it) => it.next_back(),
        }
    }
}

pub trait SharedVirtualStorage: VirtualStorage {
    /// A handle to the same physical engine. Cheap; shares all state.
    fn shared_handle(&self) -> Self;
}

/// The batch carrier: an ordered op list, accumulated by callers and
/// consumed by one engine-level `commit_batch` (ADR-0027 retired the
/// `KvBatch` trait — `batch()` returned this concrete type, so no engine
/// could ever wrap a different carrier, and `KvBatch::commit` had zero
/// callers: the commit boundary is the engine method, never the carrier).
#[derive(Default)]
pub struct Batch {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl Batch {
    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.ops.push((key, Some(value)));
    }
    pub fn del(&mut self, key: &[u8]) {
        self.ops.push((key.to_vec(), None));
    }
}

/// A batch seen as a *writer* the semantic write path can use (ADR-0031):
/// reads consult the batch's pending ops first (last op per key wins),
/// then fall through to the store; writes only accumulate into the batch.
/// Nothing reaches the engine until the caller's own `commit_batch`.
///
/// This is what lets `Collection::save_with_prev_into` run the reduce
/// fold/unfold through a batch: the read-modify-write of a group
/// accumulator needs a `get`, and `Batch` alone has none — so a
/// same-group fold would read the pre-batch value twice and double-count.
/// Read-through also covers the batch's own writes, in order.
///
/// `VirtualStorage`'s writes take `&self` (ADR-0026), so the borrowed
/// batch sits behind a `RefCell`; the caller keeps ownership and gets the
/// accumulated ops back when the adapter is dropped.
///
/// **Point reads only.** A scan that merges pending ops with the store's
/// iterator is a different operation (read, decide, then open a batch),
/// and a partial view would be worse than none: every scan shape
/// (`scan_range`, and the `scan_suffix` / `scan_range_iter` defaults that
/// funnel through it) panics with the contract instead of answering.
pub struct BatchStore<'a, S: VirtualStorage> {
    store: &'a S,
    batch: std::cell::RefCell<&'a mut Batch>,
}

impl<'a, S: VirtualStorage> BatchStore<'a, S> {
    pub fn new(store: &'a S, batch: &'a mut Batch) -> Self {
        Self {
            store,
            batch: std::cell::RefCell::new(batch),
        }
    }
}

impl<S: VirtualStorage> VirtualStorage for BatchStore<'_, S> {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()> {
        // Buffering an op cannot fail; the commit is where the engine answers.
        self.batch.borrow_mut().put(key, value);
        Ok(())
    }

    fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>> {
        // Newest op wins: a put-then-del leaves a `del` that shadows the
        // put, a del-then-put leaves the put — both answer correctly.
        let pending = self
            .batch
            .borrow()
            .ops
            .iter()
            .rev()
            .find(|(k, _)| k.as_slice() == key)
            .map(|(_, v)| v.clone());
        match pending {
            Some(v) => Ok(v),
            None => self.store.get(key),
        }
    }

    fn del(&self, key: &[u8]) -> crate::error::Result<()> {
        // Buffering an op cannot fail; the commit is where the engine answers.
        self.batch.borrow_mut().del(key);
        Ok(())
    }

    fn scan_range(
        &self,
        _begin: &[u8],
        _end: Option<&[u8]>,
    ) -> crate::error::Result<Vec<Vec<u8>>> {
        // Funnels every scan shape here: `scan_suffix` and
        // `scan_range_iter` both default through this method.
        Err(crate::error::Error::ScanThroughBatch)
    }
}

/// 异步引擎最小接口（与同步 VirtualStorage 对齐，ADR-0027：
/// 对齐的是审计后的同步面——scan_suffix_kv/batch 不在 trait 上）。
/// ADR-0028 relocated it from slatedb_backend: the aligned surface is
/// the engine contract, not a slatedb property — async senders over
/// the wire (`WireClient`) need it with no slatedb in sight.
#[allow(async_fn_in_trait)] // engine contract: impls are in-crate backends and wasm senders; ADR-0010 keeps the surface byte-only
pub trait VirtualStorageAsync {
    /// The async mirror of the sync `put`: `Err` is the engine (or the
    /// transport) refusing the bytes.
    async fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()>;
    /// The async mirror of the sync `get`: `Ok(None)` is absence, `Err` is the
    /// engine (or the transport) failing to answer.
    async fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>>;
    /// The async mirror of the sync `del`: `Ok(())` covers "nothing there".
    async fn del(&self, key: &[u8]) -> crate::error::Result<()>;
    /// 前缀扫描，返回每个 key 的"剩余段"（去掉 prefix）
    async fn scan_suffix(&self, prefix: &[u8]) -> crate::error::Result<Vec<Vec<u8>>>;
    /// Range scan over FULL keys: `[begin, end)` byte order; None end =
    /// unbounded. The async mirror of the sync trait's `scan_range`.
    async fn scan_range(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> crate::error::Result<Vec<Vec<u8>>>;
    /// Async mirror of the sync `scan_range_iter` (ADR-0027). The default
    /// BUFFERS via `scan_range` + per-key `get` (N+1); SlatedbStore
    /// overrides with a ONE-PASS materialization (see impl). Laziness is
    /// deliberately NOT attempted: the sync `ScanIter::next` would need
    /// `block_on`, and the async call site lives INSIDE a runtime —
    /// re-entrant `block_on` panics (module header). ADR-0020 already
    /// ruled the async surface small enough that buffering is harmless.
    async fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> ScanIter {
        let mut pairs: Vec<crate::error::Result<(Vec<u8>, Vec<u8>)>> = Vec::new();
        match self.scan_range(begin, end).await {
            Ok(keys) => {
                for k in keys {
                    match self.get(&k).await {
                        Ok(Some(v)) => pairs.push(Ok((k, v))),
                        Ok(None) => {}
                        Err(e) => {
                            pairs.push(Err(e));
                            break;
                        }
                    }
                }
            }
            Err(e) => pairs.push(Err(e)),
        }
        ScanIter::Buffered(pairs.into_iter())
    }
    /// Async mirror of the sync `commit_batch` (ADR-0027): one engine-level
    /// write over all accumulated ops. Default replays through put/del;
    /// engines with a native batch override (SlatedbStore ships slatedb's
    /// `WriteBatch` as ONE `db.write` + durable wait).
    async fn commit_batch(&self, batch: Batch) -> crate::error::Result<()> {
        for (k, v) in batch.ops {
            match v {
                Some(v) => self.put(k, v).await?,
                None => self.del(&k).await?,
            }
        }
        Ok(())
    }
}

/// Prefix scan returning `(key suffix, value)` pairs — the async twin of
/// the free `scan_suffix_kv` (ADR-0027; one pair-scan shape in both
/// worlds). Relocated with the trait (ADR-0028).
pub async fn scan_suffix_kv_async<S: VirtualStorageAsync + ?Sized>(
    store: &S,
    prefix: &[u8],
) -> crate::error::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let end = prefix_end(prefix);
    let mut out = Vec::new();
    for item in store.scan_range_iter(prefix, end.as_deref()).await {
        let (full, v) = item?;
        out.push((full.get(prefix.len()..).unwrap_or(&[]).to_vec(), v));
    }
    Ok(out)
}


