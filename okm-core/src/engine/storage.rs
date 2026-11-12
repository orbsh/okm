//! Storage abstraction: the sync [`VirtualStorage`] and async
//! The synchronous engine contract plus the shared batch carrier. Every backend
//! (fjall / redb / localStorage / the remote sender) implements it; the async
//! mirror and the engines that speak it live in `okm-async` (ADR-0059).
//! Unit tests run on [`crate::engine::mem_store::MemStore`]; the real-engine
//! matrix is `okm_async::TestStore`, used by integration tests.

/// Minimal KV engine interface: one ordered-read primitive (`scan_range`)
/// plus point reads and batches. The `fjall` / `redb` features
/// each provide an implementation; unit tests run on
/// [`crate::engine::mem_store::MemStore`], and the real-engine matrix is
/// `okm_async::TestStore` (ADR-0059).
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
    /// [`prefix_end`]). Engines override when they have a native prefix
    /// scan (`scan_prefix(prefix, subrange)`) that beats a range.
    ///
    /// This is one of the two names that still answers a `Vec` (the other is
    /// [`commit_batch`](Self::commit_batch)); it derives from the ONE iterator
    /// primitive, so a backend that can only scan lazily pays nothing extra to
    /// satisfy it.
    fn scan_suffix(&self, prefix: &[u8]) -> crate::error::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for item in self.scan_range(prefix, prefix_end(prefix).as_deref()) {
            let (key, _) = item?;
            // The default MUST honor the documented return shape: `scan_range`
            // hands back FULL keys, so the prefix is stripped here. A default
            // that forwarded full keys kept every tail-slicing caller working
            // (they slice from the end) while silently turning every
            // LENGTH-based judgement into a no-op — an impl with only the raw
            // ops (a host-injected engine) then reported suffixes one prefix
            // too long, and exact matching could never fire there. The range
            // holds exactly the keys carrying `prefix`, so the slice is total;
            // `get` keeps it panic-free regardless. The value is discarded:
            // a suffix scan is a key question, and the pair iterator is the
            // shape the contract has.
            out.push(key.get(prefix.len()..).unwrap_or(&[]).to_vec());
        }
        Ok(out)
    }
    /// Range scan over FULL `(key, value)` pairs: `[begin, end)` in byte
    /// order — `None` end = unbounded. Begin is inclusive. This is the
    /// ONE ordered-read primitive of the engine contract; prefix scanning
    /// is its special case (see `scan_suffix`). Byte order == value
    /// order for every OKM-encoded field (BE fixed-width, VarInt's
    /// prefix-monotonic encoding), so a `1 < a < 100` predicate IS a
    /// key interval — the engine reads only rows inside it.
    ///
    /// The answer is LAZY, pulled on demand: half-way abandonment (LIMIT,
    /// first-match) costs only the reads actually performed. The opaque
    /// [`ScanIter`] is the trait's streaming contract — every native backend
    /// iterator (fjall `Iter`, redb `OwnedRange`, an async engine's
    /// iterator, the remote page) is 'static and owned, so wrapping is free;
    /// buffers never materialize unless the consumer collects. A caller that
    /// wants the keys or the pairs as a `Vec` writes
    /// `.collect::<crate::error::Result<Vec<_>>>()` — the eager form is the
    /// caller's mirror, not a second method. Engines with no streaming
    /// source state themselves as `ScanIter::Buffered`.
    ///
    /// Errors ride the ITEMS: a constructor cannot answer for a failure the
    /// engine only discovers per key, and swallowing it is how "failed" used
    /// to read as "empty" (ADR-0038).
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter;
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
/// ONE pass over [`VirtualStorage::scan_range`] (ADR-0027 moved this
/// off the trait — its old default derived from `scan_suffix` + `get`,
/// an N+1 shape no engine ever overrode).
pub fn scan_suffix_kv<S: VirtualStorage + ?Sized>(
    store: &S,
    prefix: &[u8],
) -> crate::error::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let end = prefix_end(prefix);
    let mut out = Vec::new();
    for item in store.scan_range(prefix, end.as_deref()) {
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
/// the `shared_handle()` view IS the same physical keyspace (fjall's and
/// redb's handles are Arc-kernel and already behave this way).
/// `NestStorage` requires it — a nest wraps the engine in an
/// `Arc<Mutex<S>>`, and "wrapped" must mean shared, not copied.
/// Deep-copy-Clone engines (test maps) deliberately do not implement
/// this: a copied engine behind two hosts would silently fork the
/// keyspace.
/// The engine contract's streaming iterator, returned by
/// [`VirtualStorage::scan_range`]. An opaque enum over the
/// backends' native owned iterators rather than a `dyn` box: the enum
/// keeps `DoubleEndedIterator` — a consumer may walk the range
/// backwards (e.g. "last N entries") without a second buffer pass —
/// and [`ScanIter::seek`] repositions it.
/// Variants without a native backwards walk (an async-forward engine's
/// iterator, the remote buffered round trip) degrade to buffered
/// `next_back` over a materialized tail — semantics identical, laziness
/// lost only where the engine cannot give it back.
pub enum ScanIter {
    /// fjall: a `Snapshot` plus its cursor, so `seek` reopens the range on
    /// the same snapshot (`fjall_backend::FjallScan`), both directions.
    #[cfg(feature = "fjall")]
    Fjall(super::fjall_backend::FjallScan),
    /// redb: the `ReadTransaction` plus its `OwnedRange`, so `seek` reopens
    /// on the same snapshot (`redb_backend::RedbScan`), both directions.
    /// Boxed because this is the largest arm (the transaction and its range);
    /// the enum is created once per scan, so one allocation is cheaper than
    /// moving the whole thing around.
    #[cfg(feature = "redb")]
    Redb(Box<super::redb_backend::RedbScan>),
    /// Remote paged stream (ADR-0021): lazy over the wire, one
    /// OP_SCAN_STREAM page per refill; `next_back` degrades to buffered.
    Remote(super::nest::RemoteScanIter),
    /// Owning buffer: a non-streaming source, and any test map.
    Buffered(std::vec::IntoIter<crate::error::Result<(Vec<u8>, Vec<u8>)>>),
    /// An engine that lives OUTSIDE this crate (ADR-0059): type-erased because
    /// the iterator's type belongs to a crate core must not name. It is what
    /// keeps a scan lazy for engines that left — buffering would delete the
    /// guarantee ADR-0020 ruled, on the `LIMIT 10` over a million rows case.
    External(ExternalScan),
}

/// What an engine outside this crate hands a scan back as (ADR-0059): the
/// iterator type belongs to that crate, so the source is type-erased. The
/// wrapper adds the one thing a type-erased source cannot express — a place to
/// hold an item while a forward-only `seek` decides whether to keep it — so an
/// external engine still honors `ScanIter::seek`'s contract.
pub struct ExternalScan {
    inner: ExternalSource,
    /// One item pulled while seeking; handed out before the source.
    pending: Option<ExternalItem>,
}

/// One item a type-erased external scan yields.
type ExternalItem = crate::error::Result<(Vec<u8>, Vec<u8>)>;

/// The type-erased, forward-or-backward source behind an [`ExternalScan`].
type ExternalSource = Box<dyn DoubleEndedIterator<Item = ExternalItem> + Send>;

impl ExternalScan {
    pub fn new(inner: ExternalSource) -> Self {
        Self {
            inner,
            pending: None,
        }
    }

    /// Fallback seek: a type-erased `DoubleEndedIterator` has no cursor to
    /// move, so this drains forward, holding the first entry at or after
    /// `lower` (and any fault it meets) in `pending` so nothing is lost.
    fn seek(&mut self, lower: &[u8]) -> bool {
        while let Some(item) = self.pending.take().or_else(|| self.inner.next()) {
            match &item {
                Ok((k, _)) if k.as_slice() < lower => continue,
                // A fault is an answer, not an end: hand it out and say
                // "not exhausted".
                _ => {
                    self.pending = Some(item);
                    return true;
                }
            }
        }
        false
    }
}

impl Iterator for ExternalScan {
    type Item = crate::error::Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.pending.take().or_else(|| self.inner.next())
    }
}

impl DoubleEndedIterator for ExternalScan {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self.inner.next_back() {
            Some(item) => Some(item),
            // The held item is the only one left once the back is spent.
            None => self.pending.take(),
        }
    }
}

impl ScanIter {
    /// Reposition the scan at the first entry whose key is `>= lower`.
    ///
    /// FORWARD-ONLY: a `lower` at or behind the current position is a no-op
    /// that keeps the position. That is deliberate — a seek is a cursor move,
    /// not a re-query, so a caller walking ascending targets never pays to go
    /// back, and one walking descending targets keeps the position it has
    /// rather than silently re-reading rows it already consumed.
    ///
    /// `false` means the source has nothing at or after `lower` — the scan is
    /// exhausted. `true` means at least one item may still arrive; a fault
    /// item counts as an answer (it will surface on the next pull), never as
    /// an end. A seek that REOPENS the range (fjall, redb, remote) resets the
    /// back cursor to the range's end, so `next_back` afterwards walks the
    /// range from the top; a drain (buffered, external) has no cursor to
    /// reset and keeps the back position it already had.
    ///
    /// What each backend actually gets:
    ///
    /// - **fjall** — no native cursor seek (`fjall::Iter` is a boxed
    ///   `DoubleEndedIterator`; the crate has no `seek`), so the range is
    ///   reopened on the SAME [`Snapshot`](fjall::Snapshot) the scan was born
    ///   with. One snapshot per scan, shared by every seek.
    /// - **redb** — `OwnedRange` has no seek and redb's `Cursor` is behind its
    ///   `experimental-api-5` feature, so the range is reopened on the SAME
    ///   [`ReadTransaction`](redb::ReadTransaction) the iterator holds. This
    ///   is also why a scan opens one read transaction instead of one per
    ///   scan.
    /// - **remote** — the paged stream is repositioned and one page is
    ///   requested, so the answer is a real request and not a guess. Chunk
    ///   consistency is unchanged (ADR-0021: no snapshot across pages).
    /// - **buffered** (`ScanIter::Buffered`: mem/test maps, the slatedb sync
    ///   facade, a remote failure) — drain forward over the buffer. This is
    ///   NOT a real seek: it costs one step per skipped entry, and it exists
    ///   because a materialized `Vec` has no cursor to move. It inspects the
    ///   buffer without consuming, so a fault inside the skipped prefix still
    ///   surfaces on the next pull.
    /// - **external engines** (ADR-0059) — the same forward drain over the
    ///   type-erased source, holding the first hit so it is not lost.
    pub fn seek(&mut self, lower: &[u8]) -> bool {
        match self {
            #[cfg(feature = "fjall")]
            ScanIter::Fjall(it) => it.seek(lower),
            #[cfg(feature = "redb")]
            ScanIter::Redb(it) => it.seek(lower),
            ScanIter::Remote(it) => it.seek(lower),
            ScanIter::Buffered(it) => buffered_seek(it, lower),
            ScanIter::External(it) => it.seek(lower),
        }
    }
}

/// The buffered fallback: skip the leading entries that sort before `lower`
/// without consuming them (`as_slice` gives the lookahead), then report
/// whether anything is left. A fault in the skipped prefix stops the walk so
/// the next pull still reports it.
fn buffered_seek(
    it: &mut std::vec::IntoIter<crate::error::Result<(Vec<u8>, Vec<u8>)>>,
    lower: &[u8],
) -> bool {
    let mut skip = 0usize;
    for item in it.as_slice() {
        match item {
            Ok((k, _)) if k.as_slice() < lower => skip += 1,
            _ => break,
        }
    }
    if skip > 0 {
        it.nth(skip - 1);
    }
    !it.as_slice().is_empty()
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
            ScanIter::Fjall(it) => it.next(),
            #[cfg(feature = "redb")]
            ScanIter::Redb(it) => it.next(),
            ScanIter::Remote(it) => it.next(),
            ScanIter::Buffered(it) => it.next(),
            ScanIter::External(it) => it.next(),
        }
    }
}

impl DoubleEndedIterator for ScanIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            #[cfg(feature = "fjall")]
            ScanIter::Fjall(it) => it.next_back(),
            #[cfg(feature = "redb")]
            ScanIter::Redb(it) => it.next_back(),
            ScanIter::Remote(it) => it.next_back(),
            ScanIter::Buffered(it) => it.next_back(),
            ScanIter::External(it) => it.next_back(),
        }
    }
}

pub trait SharedVirtualStorage: VirtualStorage {
    /// A handle to the same physical engine. Cheap; shares all state.
    fn shared_handle(&self) -> Self;
}

/// A BORROWED store is a store: `&S` forwards every method, so several
/// collections (and a caller's batch) can sit over ONE engine handle without
/// cloning it. Writes already take `&self` (ADR-0026); this is the type-level
/// half of the same claim, and it is what lets one capturing store
/// ([`crate::changelog::ChangelogStore`]) serve every collection in a process —
/// two decorators over one directory would be two writers on one segment number.
impl<S: VirtualStorage + ?Sized> VirtualStorage for &S {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()> {
        (**self).put(key, value)
    }

    fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>> {
        (**self).get(key)
    }

    fn del(&self, key: &[u8]) -> crate::error::Result<()> {
        (**self).del(key)
    }

    fn scan_suffix(&self, prefix: &[u8]) -> crate::error::Result<Vec<Vec<u8>>> {
        (**self).scan_suffix(prefix)
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        (**self).scan_range(begin, end)
    }

    fn commit_batch(&self, batch: Batch) -> crate::error::Result<()> {
        (**self).commit_batch(batch)
    }
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
/// (`scan_range`, and `scan_suffix` which derives from it) answers the
/// contract's one failure item instead of a view.
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

    fn scan_range(&self, _begin: &[u8], _end: Option<&[u8]>) -> ScanIter {
        // Every scan shape funnels through here; the contract's error rides
        // the ITEM, so a scan over a batch answers one failure instead of a
        // partial view.
        ScanIter::Buffered(vec![Err(crate::error::Error::ScanThroughBatch)].into_iter())
    }
}
