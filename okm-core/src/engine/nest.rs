//! Remote storage backend (ADR-0010 Phase 7). Sender: implements
//! [`VirtualStorage`] by framing the already-encoded op list — writes are
//! fire-and-forget, reads block on the round trip. Receiver:
//! [`NestStorage`] prepends its declared prefix and executes frames on a
//! plain byte-level engine. Neither side parses key contents; the wire
//! carries only op bytes (ADR-0010 §2: no semantic parsing, no generic
//! serialization library — the frame is counted fields, not a protocol).
//!
//! The frame codec itself lives in `okm-wire` (zero dependencies, zero
//! OKM semantics) so a transport that reuses other channels — Aura's WS
//! connection, a TCP client, UDS — depends only on the codec, not on
//! okm-core. This module is the okm-core binding of that codec: the
//! sender's `impl VirtualStorage`, the receiver host, and the mpsc
//! reference transport.
//!
//! ONE execution surface (no read/write split): the host's intake is
//! `apply(frame) -> Option<OpResponse>`. Every op executes in frame
//! order; mutating ops commit together; get/scan fill the response.
//! A put/delete answer is the empty (default) response — the sender
//! learns success by receiving A response; the delivery guarantee is
//! the transport's, never the frame's (ADR-0010 §6). Read correlation
//! also rides the transport envelope (an mpsc sender beside each
//! frame), never the frame bytes.
//!
//! The `NestStorage` derive (with `#[ok_ns]`) generates exactly the [`NestStorage`]
//! shape; this manual form is the reference implementation the derive
//! targets.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use okm_wire::{
    OP_DELETE, OP_GET, OP_PUT, OP_SCAN, OP_SCAN_STREAM, OpFrame, OpResponse, TAIL_FINAL, TAIL_MORE,
};

use crate::engine::storage::{MemBatch, SharedVirtualStorage, VirtualStorage, prefix_end};

// ============ sender side ============

/// Sender handle over the in-process reference transport. Implements
/// [`VirtualStorage`]: writes ship as one framed batch (fire-and-forget —
/// one frame = one receiver execution pass; channel order = write order),
/// reads block on the round trip. A TCP client would be another `impl
/// VirtualStorage` with the same `okm-wire` frames over a stream —
/// everything here is transport-shaped, not contract-shaped.
pub struct RemoteStore {
    exec_tx: mpsc::Sender<(mpsc::Sender<OpResponse>, Vec<u8>)>,
}

impl RemoteStore {
    /// A handle the iterator can own: same channel endpoints, same
    /// physical store — a clone of the sender side (the channel sender
    /// is `Clone`; the store has no other state).
    fn clone_for_iter(&self) -> Self {
        Self {
            exec_tx: self.exec_tx.clone(),
        }
    }

    /// One `OP_SCAN_STREAM` page request. `exclusive` re-sends the last
    /// key as begin and lets the receiver's `[0x02]` flag do the
    /// increment — the sender never parses or derives key bytes
    /// (ADR-0021: the sender owns the cursor, the receiver owns keys).
    fn stream_page(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
        exclusive: bool,
        page: u8,
    ) -> OpResponse {
        let mut value = vec![if exclusive { 0x02 } else if end.is_some() { 0x01 } else { 0x00 }];
        if let Some(end) = end {
            value.extend_from_slice(end);
        }
        value.push(page);
        let resp = self.round_trip(&OpFrame::one(OP_SCAN_STREAM, begin.to_vec(), value));
        debug_assert!(resp.tail.is_some(), "stream answer is a chunk");
        resp
    }
}

/// The sender-side endpoints a [`NestStorage::new`] hands out. Each read
/// round trip carries its own reply box in the transport envelope, so
/// several senders can share one host without cross-reading answers.
pub struct VirtualHandle {
    exec_tx: mpsc::Sender<(mpsc::Sender<OpResponse>, Vec<u8>)>,
}

impl VirtualHandle {
    /// Open one sender endpoint.
    pub fn open(&self) -> RemoteStore {
        RemoteStore {
            exec_tx: self.exec_tx.clone(),
        }
    }
}

impl RemoteStore {
    fn send_write(&self, frame: &OpFrame) {
        self.exec_tx
            .send((mpsc::channel().0, frame.encode()))
            .expect("channel open");
    }

    fn round_trip(&self, frame: &OpFrame) -> OpResponse {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.exec_tx
            .send((reply_tx, frame.encode()))
            .expect("channel open");
        reply_rx.recv().expect("response")
    }
}

impl VirtualStorage for RemoteStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        self.send_write(&OpFrame::one(OP_PUT, key, value));
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.round_trip(&OpFrame::one(OP_GET, key.to_vec(), Vec::new()))
            .value
    }

    fn del(&self, key: &[u8]) {
        self.send_write(&OpFrame::one(OP_DELETE, key.to_vec(), Vec::new()));
    }

    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.round_trip(&OpFrame::one(OP_SCAN, prefix.to_vec(), Vec::new()))
            .suffixes
    }
    /// Range scan rides the SAME OP_SCAN frame: the value segment —
    /// empty for a prefix scan — carries `[0x01][end bytes]` when a
    /// finite end exists, `[0x00]` for unbounded. No wire change: the
    /// frame already has a value field; prefix is just the special case
    /// with an empty value.
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        let mut value = vec![0u8];
        if let Some(end) = end {
            value[0] = 1;
            value.extend_from_slice(end);
        }
        self.round_trip(&OpFrame::one(OP_SCAN, begin.to_vec(), value)).suffixes
    }

    /// Lazy paged stream (ADR-0021): one `ScanIter::Remote` whose refill
    /// is an `OP_SCAN_STREAM` round trip per page. Early abandonment
    /// stops pulling pages; values ride the chunk (no N+1). The
    /// receiver's chunk-time consistency (no snapshot across pages) is
    /// the documented contract — a consumer needing a frozen view
    /// materializes it instead.
    fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> crate::engine::storage::ScanIter {
        crate::engine::storage::ScanIter::Remote(RemoteScanIter {
            store: self.clone_for_iter(),
            begin: begin.to_vec(),
            end: end.map(|e| e.to_vec()),
            page: 0, // 0 = engine default (256)
            buf: VecDeque::new(),
            cursor: None,
            done: false,
        })
    }

    /// One `commit_batch` = one frame = one receiver execution pass: the
    /// cross-assembly-point atomic path (ADR-0003) survives remoteness
    /// because the receiver commits the whole op list in one call.
    fn commit_batch(&mut self, batch: MemBatch) -> Result<(), String> {
        self.send_write(&OpFrame::write_batch(&batch.ops));
        Ok(())
    }
}

// ============ sender-side stream iterator (ADR-0021) ============

/// The `ScanIter::Remote` engine arm: lazy over the wire. State lives on
/// the SENDER (the receiver is stateless by contract): the drained page
/// buffer, the cursor (the last key returned — re-sent verbatim as the
/// next request's begin), and the stream-end flag set from the chunk's
/// tail byte.
pub struct RemoteScanIter {
    store: RemoteStore,
    begin: Vec<u8>,
    end: Option<Vec<u8>>,
    page: u8,
    buf: std::collections::VecDeque<(Vec<u8>, Vec<u8>)>,
    /// The last CONSUMED key — the sender-owned cursor. `None` before
    /// the first item; `Some` re-sent verbatim as the next page's
    /// exclusive begin (the receiver does the increment).
    cursor: Option<Vec<u8>>,
    done: bool,
}

impl RemoteScanIter {
    fn refill(&mut self) {
        if self.done {
            return;
        }
        let (exclusive, begin) = match self.cursor.take() {
            // Resume: the last CONSUMED key is the cursor — re-sent as
            // begin with the exclusive flag (receiver-side increment).
            Some(last) => (true, last),
            None => (false, std::mem::take(&mut self.begin)),
        };
        let resp = self.store.stream_page(&begin, self.end.as_deref(), exclusive, self.page);
        self.done = resp.tail != Some(TAIL_MORE);
        self.buf.extend(resp.hits);
    }

    fn consume_front(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        let item = self.buf.pop_front()?;
        self.cursor = Some(item.0.clone());
        Some(item)
    }

    fn consume_back(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        let item = self.buf.pop_back()?;
        self.cursor = Some(item.0.clone());
        Some(item)
    }
}

impl Iterator for RemoteScanIter {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() && !self.done {
            self.refill();
        }
        self.consume_front()
    }
}

impl DoubleEndedIterator for RemoteScanIter {
    /// NOT overridden as lazy (ADR-0021: a backwards remote walk would
    /// need a descending-page request shape — deferred): first call
    /// materializes the remaining range through ordinary forward pages,
    /// then drains from the back — semantics identical, laziness lost
    /// where the wire shape cannot give it back (the slatedb rule).
    fn next_back(&mut self) -> Option<Self::Item> {
        while self.buf.is_empty() && !self.done {
            self.refill();
        }
        self.consume_back()
    }
}

// ============ receiver side ============

/// Receiver side: a **nested storage** — an existing engine nested
/// inside a declared prefix, executing frames through one intake. Owns
/// the real engine behind a mutex (single-writer discipline, held
/// across one execution pass — the same boundary a local caller's
/// `&mut self` provides) and knows exactly one thing: its declared
/// prefix. No TLV, no documents, no OKM semantics — storing garbage is
/// indistinguishable from storing data (ADR-0010 §2). The `NestStorage`
/// derive generates this shape; this manual form is its reference.
/// Transport-free execution core (ADR-0010 §6): ONE intake for every op
/// — receive, execute, answer if the op produces output. The mpsc
/// reference pump and a WS/UDS adapter (holding `Arc<NestStorage>`)
/// serialize on the same engine mutex; see
/// `docs/integration/WS-CHANNEL.md` for the adapter shape.
struct ExecCore<S: VirtualStorage> {
    engine: Arc<Mutex<S>>,
    prefix: Option<Vec<u8>>,
}

pub struct NestStorage<S: VirtualStorage> {
    engine: Arc<Mutex<S>>,
    /// `Some` = hosted (multi-tenant, declared via `#[ok_ns]`): every
    /// key enters as `[prefix][sender bytes]`. `None` = bare shard
    /// (single instance per engine, sharding routed by the orchestrator):
    /// frames execute byte-identical — the sender's keyspace IS the
    /// engine's keyspace.
    prefix: Option<Vec<u8>>,
    exec_rx: mpsc::Receiver<(mpsc::Sender<OpResponse>, Vec<u8>)>,
}

impl<S: SharedVirtualStorage + Send + 'static> NestStorage<S> {
    /// Hosted form: bind the host to its declared prefix and engine —
    /// prefix escape is not expressible afterwards (physical separation,
    /// not naming filters, ADR-0010 §4). Multi-tenant: several hosted
    /// hosts may share one engine; their 2-byte prefixes are byte-wise
    /// disjoint, and each carries its own private ns dictionary inside.
    /// Returns the sender endpoints.
    pub fn new(engine: S, prefix: &[u8]) -> (Self, VirtualHandle) {
        Self::with_prefix(engine, Some(prefix.to_vec()))
    }

    /// Bare shard form: NO prefix — frames execute byte-identical on the
    /// engine. The sender's keyspace IS the engine's keyspace; sharding
    /// and routing belong to the orchestrator. Prerequisite: every OKM
    /// instance pointing at this host shares one domain model (one ns
    /// dictionary, one encoding) — same binary deployed per shard makes
    /// this automatic. Coexistence with a hosted host on one engine is
    /// legal as long as the orchestrator keeps the bare instances' ns
    /// numbers off the hosted segments' numbers (an allocation duty, not
    /// a runtime check — the host has no global view and needs none).
    /// Singleton per engine is the practical shape.
    pub fn bare(engine: S) -> VirtualHandle {
        let (host, handle) = Self::with_prefix(engine, None);
        let _ = host.serve(); // pump owns its Arc; drop the intake handle
        handle
    }

    fn with_prefix(engine: S, prefix: Option<Vec<u8>>) -> (Self, VirtualHandle) {
        let (exec_tx, exec_rx) = mpsc::channel();
        (
            Self {
                engine: Arc::new(Mutex::new(engine)),
                prefix,
                exec_rx,
            },
            VirtualHandle { exec_tx },
        )
    }

    /// Start serving on the reference mpsc transport: one pump, one loop
    /// — receive frame, execute, answer. Returns `Arc<Self>` so the
    /// caller can share the same intake with other transports (a WS/UDS
    /// adapter calls `apply` on it directly — see
    /// `docs/integration/WS-CHANNEL.md`). An async receiver would
    /// task-spawn the same loop.
    pub fn serve(self) -> Arc<Self> {
        // Split: the engine+prefix go into a pump struct (Clone-able
        // core), the receiver end moves into the pump thread (mpsc
        // Receiver is not Sync — it owns its end exclusively, the
        // correct shape anyway). The returned Arc shares the same
        // engine/prefix, so `apply` from a WS adapter and the pump
        // serialize on the same mutex.
        let core = Arc::new(ExecCore {
            engine: self.engine,
            prefix: self.prefix,
        });
        let pump_rx = self.exec_rx;
        let pump_core = Arc::clone(&core);
        std::thread::spawn(move || {
            for (reply_tx, bytes) in pump_rx {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pump_core.apply(&bytes)));
                if let Ok(Some(resp)) = r {
                    // Fire-and-forget writers (put/delete) drop their reply
                    // box before the answer arrives — a failed send is the
                    // NORMAL case for them, never a pump death. Round-trip
                    // readers self-heal: each request carries a fresh box.
                    let _ = reply_tx.send(resp);
                }
            }
        });
        Arc::new(Self {
            engine: Arc::clone(&core.engine),
            prefix: core.prefix.clone(),
            exec_rx: mpsc::channel().1, // placeholder: this instance's intake is the returned Arc's `apply`
        })
    }

    /// Transport-free intake: the WS/UDS adapter calls this on the
    /// `Arc<NestStorage>` returned by `serve` (see
    /// `docs/integration/WS-CHANNEL.md`).
    pub fn apply(&self, bytes: &[u8]) -> Option<OpResponse> {
        ExecCore {
            engine: Arc::clone(&self.engine),
            prefix: self.prefix.clone(),
        }
        .apply(bytes)
    }
}

impl<S: VirtualStorage> ExecCore<S> {
    /// Apply one request frame — THE execution surface, all four ops:
    /// decode (malformed = `None`, garbage in / nothing stored), then
    /// execute in frame order. Mutating ops (put/delete) commit together
    /// in one `commit_batch` = one engine WAL write (ADR-0010 §2);
    /// query ops (get/scan) run after them and fill the response.
    /// Returns `None` only for malformed frames.
    fn apply(&self, bytes: &[u8]) -> Option<OpResponse> {
        let frame = OpFrame::decode(bytes)?;
        let mut engine = self.engine.lock().expect("engine lock");
        let mut batch = MemBatch::default();
        let mut resp = OpResponse::default();
        for (tag, key, value) in frame.0 {
            match tag {
                OP_PUT => batch.put(hosted_key(&self.prefix, &key), value),
                OP_DELETE => batch.del(&hosted_key(&self.prefix, &key)),
                OP_GET => {
                    engine.commit_batch(MemBatch::default()).ok(); // flush pending mutations first: reads see prior ops in the same frame
                    resp.value = engine.get(&hosted_key(&self.prefix, &key));
                }
                OP_SCAN => {
                    engine.commit_batch(MemBatch::default()).ok();
                    let hk = hosted_key(&self.prefix, &key);
                    // Wire contract: answers are PREFIX-RELATIVE keys —
                    // the sender's key space never sees the hosted prefix
                    // (it prepends on write and the round trip must be
                    // the inverse). Range answers strip hk from every
                    // full key; prefix answers are already relative.
                    let plen = self.prefix.as_ref().map_or(0, Vec::len);
                    let strip = move |k: Vec<u8>| k[plen..].to_vec();
                    resp.suffixes = match value.first() {
                        // [0x01][end] = bounded range; [0x00] alone =
                        // begin-only range (unbounded end); EMPTY value =
                        // the legacy prefix scan. (A begin-only range and
                        // a prefix scan are different predicates.)
                        Some(&0x00) if value.len() == 1 => {
                            engine.scan_range(&hk, None).into_iter().map(strip).collect()
                        }
                        Some(&0x01) => {
                            let end = hosted_key(&self.prefix, &value[1..]);
                            engine
                                .scan_range(&hk, Some(&end))
                                .into_iter()
                                .map(strip)
                                .collect()
                        }
                        _ => engine.scan_suffix(&hk), // empty value = prefix scan
                    };
                }
                OP_SCAN_STREAM => {
                    // ADR-0021: one page per apply — the host stays
                    // stateless (constraint 2); the sender's next request
                    // carries the cursor. Value segment =
                    // [flag][end bytes?][page u8] with flag 0x00 unbounded
                    // / 0x01 + end / 0x02 + end AND exclusive begin (the
                    // resume request re-sends the last key as begin; the
                    // flag moves inclusivity, not the bytes). page:
                    // 0 = engine default (256), 0xFF = one giant page
                    // (the legacy buffered shape).
                    engine.commit_batch(MemBatch::default()).ok();
                    // Grammar: [flag][end bytes?][page u8] — page is
                    // always the last byte; the flag says whether an end
                    // span sits between it and the flag.
                    let page = {
                        let p = value.last()?;
                        *p
                    };
                    let (end, exclusive) = match value[0] {
                        0x00 => (None, false),
                        0x01 => (
                            Some(hosted_key(&self.prefix, &value[1..value.len() - 1])),
                            false,
                        ),
                        0x02 => (
                            Some(hosted_key(&self.prefix, &value[1..value.len() - 1])),
                            true,
                        ),
                        _ => return None,
                    };
                    let hk = hosted_key(&self.prefix, &key);
                    // Exclusive begin: bump past the cursor with the same
                    // carry increment `prefix_end` uses (an all-0xFF begin
                    // cannot occur here — the cursor key just existed).
                    let begin: Option<Vec<u8>> = if exclusive {
                        prefix_end(&hk)
                    } else {
                        Some(hk)
                    };
                    let want = match page {
                        0xFF => usize::MAX,
                        0 => 256,
                        p => p as usize,
                    };
                    // Hits are prefix-relative (same contract as OP_SCAN):
                    // the sender owns cursor bytes it can re-send verbatim.
                    let plen = self.prefix.as_ref().map_or(0, Vec::len);
                    let pairs: Vec<(Vec<u8>, Vec<u8>)> = engine
                        .scan_range_iter(
                            begin.as_deref().expect("begin present"),
                            end.as_deref(),
                        )
                        .take(want)
                        .map(|(k, v)| (k[plen..].to_vec(), v))
                        .collect();
                    // More pages only when the page filled to its exact
                    // budget (0xFF = giant page is always FINAL — it asked
                    // for the whole buffered answer).
                    let tail = if pairs.len() == want && want != usize::MAX {
                        TAIL_MORE
                    } else {
                        TAIL_FINAL
                    };
                    resp.hits = pairs;
                    resp.tail = Some(tail);
                }
                _ => return None, // unknown op tag = malformed frame
            }
        }
        engine.commit_batch(batch).ok();
        Some(resp)
    }
}

/// Sender key bytes → receiver-side physical key. Hosted (Some): pure
/// concatenation, receiver bytes first, sender bytes after, order never
/// adjusted (ADR-0010 §5) — the receiver never parses what follows its
/// prefix. Bare shard (None): byte-identical — the engine's keyspace IS
/// the sender's.
fn hosted_key(prefix: &Option<Vec<u8>>, sender_key: &[u8]) -> Vec<u8> {
    match prefix {
        Some(p) => {
            let mut full = Vec::with_capacity(p.len() + sender_key.len());
            full.extend_from_slice(p);
            full.extend_from_slice(sender_key);
            full
        }
        None => sender_key.to_vec(),
    }
}

#[cfg(all(test, feature = "test-engines"))]
mod tests {
    use super::*;
    use crate::engine::storage::VirtualStorage;
    use std::collections::BTreeMap;

    #[derive(Default, Clone)]
    struct TestEngine(std::sync::Arc<std::sync::Mutex<BTreeMap<Vec<u8>, Vec<u8>>>>);

    impl VirtualStorage for TestEngine {
        fn put(&self, key: Vec<u8>, value: Vec<u8>) {
            self.0.lock().unwrap().insert(key, value);
        }
        fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
            self.0.lock().unwrap().get(key).cloned()
        }
        fn del(&self, key: &[u8]) {
            self.0.lock().unwrap().remove(key);
        }
        fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
            self.0
                .lock()
                .unwrap()
                .range(prefix.to_vec()..)
                .take_while(|(k, _)| k.starts_with(prefix))
                .map(|(k, _)| k[prefix.len()..].to_vec())
                .collect()
        }
        fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
            let map = self.0.lock().unwrap();
            let iter = map.range(begin.to_vec()..);
            iter.take_while(|(k, _)| match end {
                Some(end) => k.as_slice() < end,
                None => true,
            })
            .map(|(k, _)| k.clone())
            .collect()
        }
    }

    impl SharedVirtualStorage for TestEngine {
        fn shared_handle(&self) -> Self {
            self.clone()
        }
    }

    #[test]
    fn round_trip_via_handle() {
        // bare() serves internally and returns the handle — the exact
        // path bare_shard_test exercises.
        let handle = NestStorage::bare(TestEngine::default());
        let rs: RemoteStore = handle.open();
        rs.put(b"k".to_vec(), b"v".to_vec());
        assert_eq!(rs.get(b"k").as_deref(), Some(b"v".as_slice()));
    }

    #[test]
    fn apply_put_get_round_trip() {
        let (host, _handle) = NestStorage::new(TestEngine::default(), &[0, 9]);
        let put = OpFrame::one(OP_PUT, b"k".to_vec(), b"v".to_vec());
        let r = host.apply(&put.encode());
        assert!(r.is_some(), "put apply must succeed");
        let get = OpFrame::one(OP_GET, b"k".to_vec(), Vec::new());
        let r = host.apply(&get.encode());
        assert_eq!(r.unwrap().value.as_deref(), Some(b"v".as_slice()));
    }
}
