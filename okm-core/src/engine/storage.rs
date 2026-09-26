//! Storage abstraction: the sync [`VirtualStorage`] trait — the engine
//! contract every backend (fjall / slatedb / redb) implements. Tests run
//! against real engines via [`crate::engine::test_engine::TestStore`].



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
    fn put(&self, key: Vec<u8>, value: Vec<u8>);
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn del(&self, key: &[u8]);
    /// Prefix scan; returns each matching key's "suffix" (prefix removed).
    /// A convenience bound over [`scan_range`](Self::scan_range): the
    /// physical operation is the same ordered iteration, the prefix is
    /// the range whose end is the prefix's terminator (see
    /// [`prefix_end`]). Engines override when their native prefix scan
    /// beats a range (`slatedb`'s `scan_prefix(prefix, subrange)`).
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.scan_range(prefix, prefix_end(prefix).as_deref())
    }
    /// Range scan over FULL keys: `[begin, end)` in byte order —
    /// `None` end = unbounded. Begin is inclusive. This is the ONE
    /// ordered-read primitive of the engine contract; prefix scanning
    /// is its special case (see `scan_suffix`). Byte order == value
    /// order for every OKM-encoded field (BE fixed-width, VarInt's
    /// prefix-monotonic encoding), so a `1 < a < 100` predicate IS a
    /// key interval — the engine reads only rows inside it.
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>>;

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
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = self
            .scan_range(begin, end)
            .into_iter()
            .filter_map(|k| Some((k.clone(), self.get(&k)?)))
            .collect();
        ScanIter::Buffered(pairs.into_iter())
    }
    /// Prefix scan returning `(key suffix, value)` pairs, in key order,
    /// is NOT a trait method — it lives as the free function
    /// [`scan_suffix_kv`] (ADR-0027).
    /// Commit an accumulated [`MemBatch`]: one engine-level write over
    /// all ops in the list. This is the cross-collection atomicity
    /// primitive (ADR-0003): single-collection writes are already atomic
    /// inside `Collection::put`; primary + index across two assembly
    /// points share one batch and commit once. Default replays the op
    /// list through put/del (single-threaded, so the sequence is atomic
    /// within the caller's write order); engines with a native batch API
    /// (fjall `Batch`, redb one write txn) override to execute the list
    /// as ONE engine-level atomic unit — the single WAL commit is real,
    /// not a replay.
    fn commit_batch(&mut self, batch: MemBatch) -> Result<(), String> {
        for (k, v) in batch.ops {
            match v {
                Some(v) => self.put(k, v),
                None => self.del(&k),
            }
        }
        Ok(())
    }
}

/// Prefix scan returning `(key suffix, value)` pairs, in key order:
/// ONE pass over [`VirtualStorage::scan_range_iter`] (ADR-0027 moved this
/// off the trait — its old default derived from `scan_suffix` + `get`,
/// an N+1 shape no engine ever overrode).
pub fn scan_suffix_kv<S: VirtualStorage>(
    store: &S,
    prefix: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let end = prefix_end(prefix);
    store
        .scan_range_iter(prefix, end.as_deref())
        .map(|(full, v)| {
            (
                full.get(prefix.len()..).unwrap_or(&[]).to_vec(),
                v,
            )
        })
        .collect()
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
    Buffered(std::vec::IntoIter<(Vec<u8>, Vec<u8>)>),
}

impl Iterator for ScanIter {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            #[cfg(feature = "fjall")]
            ScanIter::Fjall(it) => it
                .next()
                .map(|g| {
                    let (k, v) = g
                        .into_inner()
                        .unwrap_or_else(|e| panic!("fjall iter guard: {e}"));
                    (k.to_vec(), v.to_vec())
                }),
            #[cfg(feature = "redb")]
            ScanIter::Redb(it) => it
                .next()
                .map(|r| {
                    let (k, v) = r.expect("redb range item");
                    (k.value().to_vec(), v.value().to_vec())
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
            ScanIter::Fjall(it) => it
                .next_back()
                .map(|g| {
                    let (k, v) = g
                        .into_inner()
                        .unwrap_or_else(|e| panic!("fjall iter guard: {e}"));
                    (k.to_vec(), v.to_vec())
                }),
            #[cfg(feature = "redb")]
            ScanIter::Redb(it) => it
                .next_back()
                .map(|r| {
                    let (k, v) = r.expect("redb range item");
                    (k.value().to_vec(), v.value().to_vec())
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
pub struct MemBatch {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl MemBatch {
    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.ops.push((key, Some(value)));
    }
    pub fn del(&mut self, key: &[u8]) {
        self.ops.push((key.to_vec(), None));
    }
}


