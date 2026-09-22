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
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>);
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn del(&mut self, key: &[u8]);
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
    /// Prefix scan returning `(key suffix, value)` pairs, in key order.
    /// Default derives from `scan_suffix` + `get` (two lookups per hit);
    /// engines override with a native pair scan when it matters.
    fn scan_suffix_kv(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.scan_suffix(prefix)
            .into_iter()
            .map(|sfx| {
                let full = [prefix, sfx.as_slice()].concat();
                let v = self.get(&full).unwrap_or_default();
                (sfx, v)
            })
            .collect()
    }
    /// Open a write batch: [`KvBatch`] entries accumulate here, one
    /// `commit()` = one engine-level WAL commit. This is the
    /// cross-collection atomicity primitive (ADR-0003): single-collection
    /// writes are already atomic inside `Collection::put`; primary + index
    /// across two assembly points share one batch and commit once.
    /// Default carrier is [`MemBatch`] (op list replayed by the engine
    /// trait's own put/del); engines with a native batch API (fjall
    /// `Batch`) override to wrap theirs — the single WAL commit is real,
    /// not a replay.
    fn batch(&mut self) -> MemBatch {
        MemBatch::default()
    }
    /// Commit a batch: one engine-level write over all accumulated ops.
    /// Default replays the op list through put/del (single-threaded, so
    /// the sequence is atomic within the caller's write order); engines
    /// with a native batch override this to hand the carrier to the
    /// engine's own commit (one real WAL write).
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

/// Engine-agnostic write batch: accumulates put/delete operations that
/// commit together in one engine-level WAL write (ADR-0003).
pub trait KvBatch {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>);
    fn del(&mut self, key: &[u8]);
    /// Commit atomically: one WAL write over everything accumulated.
    fn commit(self) -> Result<(), String>;
}

/// The default batch carrier: an ordered op list. Commit is a no-op on
/// the carrier itself — the caller replays via the engine trait's own
/// put/del; see [`VirtualStorage::batch`].
#[derive(Default)]
pub struct MemBatch {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl KvBatch for MemBatch {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.ops.push((key, Some(value)));
    }
    fn del(&mut self, key: &[u8]) {
        self.ops.push((key.to_vec(), None));
    }
    fn commit(self) -> Result<(), String> {
        Ok(())
    }
}


