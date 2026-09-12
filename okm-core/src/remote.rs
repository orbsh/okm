//! Remote storage backend (ADR-0010 Phase 7). Sender: implements
//! [`VirtualStorage`] by framing the already-encoded op list —
//! fire-and-forget writes, round-trip reads. Receiver: [`StorageHost`]
//! prepends its declared prefix and replays frames on a plain byte-level
//! engine. Neither side parses key contents; the wire carries only op
//! bytes (ADR-0010 §2: no semantic parsing, no generic serialization
//! library — the frame is counted fields, not a protocol).
//!
//! The frame codec itself lives in `okm-wire` (zero dependencies, zero
//! OKM semantics) so a transport that reuses other channels — Aura's WS
//! connection, a TCP client, UDS — depends only on the codec, not on
//! okm-core. This module is the okm-core binding of that codec: the
//! sender's `impl VirtualStorage`, the receiver host, and the mpsc
//! reference transport. Read correlation rides the transport envelope
//! (an mpsc sender beside each frame), never the frame bytes (ADR-0010
//! §6). The `#[kv_storage]` derive generates exactly the [`StorageHost`]
//! shape; this manual form is the reference implementation the derive
//! targets.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use okm_wire::{OP_DELETE, OP_PUT, ReadFrame, ReadResponse, WriteFrame};

use crate::storage::{KvBatch, MemBatch, VirtualStorage};

// ============ sender side ============

/// Sender handle over the in-process reference transport. Implements
/// [`VirtualStorage`]: writes ship as one framed batch (fire-and-forget —
/// one frame = one receiver WAL commit; channel order = write order),
/// reads block on the round trip. A TCP client would be another `impl
/// VirtualStorage` with the same `okm-wire` frames over a stream —
/// everything here is transport-shaped, not contract-shaped.
pub struct RemoteStore {
    write_tx: mpsc::Sender<Vec<u8>>,
    read_tx: mpsc::Sender<(mpsc::Sender<ReadResponse>, Vec<u8>)>,
}

/// The sender-side endpoints a [`StorageHost::new`] hands out. Each read
/// round trip carries its own reply box in the transport envelope, so
/// several senders can share one host without cross-reading answers.
pub struct VirtualHandle {
    write_tx: mpsc::Sender<Vec<u8>>,
    read_tx: mpsc::Sender<(mpsc::Sender<ReadResponse>, Vec<u8>)>,
}

impl VirtualHandle {
    /// Open one sender endpoint.
    pub fn open(&self) -> RemoteStore {
        RemoteStore {
            write_tx: self.write_tx.clone(),
            read_tx: self.read_tx.clone(),
        }
    }
}

impl RemoteStore {
    fn send_write(&self, frame: &WriteFrame) {
        self.write_tx
            .send(frame.encode())
            .expect("write channel open");
    }

    fn round_trip(&self, frame: &ReadFrame) -> ReadResponse {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.read_tx
            .send((reply_tx, frame.encode()))
            .expect("read channel open");
        reply_rx.recv().expect("read response")
    }
}

impl VirtualStorage for RemoteStore {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.send_write(&WriteFrame::new(vec![(OP_PUT, key, value)]));
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.round_trip(&ReadFrame::Get { key: key.to_vec() })
            .value
    }

    fn del(&mut self, key: &[u8]) {
        self.send_write(&WriteFrame::new(vec![(OP_DELETE, key.to_vec(), Vec::new())]));
    }

    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.round_trip(&ReadFrame::Scan {
            prefix: prefix.to_vec(),
        })
        .suffixes
    }

    /// Batch = carrier accumulate only; the frame ships at commit.
    fn batch(&mut self) -> MemBatch {
        MemBatch::default()
    }

    /// One `commit_batch` = one frame = one receiver WAL write: the
    /// cross-assembly-point atomic path (ADR-0003) survives remoteness
    /// because the receiver commits the whole op list in one call.
    fn commit_batch(&mut self, batch: MemBatch) -> Result<(), String> {
        let ops = batch
            .ops
            .into_iter()
            .map(|(k, v)| match v {
                Some(v) => (OP_PUT, k, v),
                None => (OP_DELETE, k, Vec::new()),
            })
            .collect();
        self.send_write(&WriteFrame::new(ops));
        Ok(())
    }
}

// ============ receiver side ============

/// Receiver host: owns the real engine behind a mutex (single-writer
/// discipline, held across one `commit_batch` — the same boundary a local
/// caller's `&mut self` provides) and knows exactly one thing: its
/// declared prefix. No TLV, no rows, no OKM semantics — storing garbage is
/// indistinguishable from storing data (ADR-0010 §2). The `#[kv_storage]`
/// derive generates this shape; this manual form is its reference.
pub struct StorageHost<S: VirtualStorage> {
    engine: Arc<Mutex<S>>,
    prefix: Vec<u8>,
    write_rx: mpsc::Receiver<Vec<u8>>,
    read_rx: mpsc::Receiver<(mpsc::Sender<ReadResponse>, Vec<u8>)>,
}

impl<S: VirtualStorage + Send + 'static> StorageHost<S> {
    /// Bind the host to its declared prefix and engine at construction —
    /// prefix escape is not expressible afterwards (physical separation,
    /// not naming filters, ADR-0010 §4). Returns the sender endpoints.
    pub fn new(engine: S, prefix: &[u8]) -> (Self, VirtualHandle) {
        let (write_tx, write_rx) = mpsc::channel();
        let (read_tx, read_rx) = mpsc::channel();
        (
            Self {
                engine: Arc::new(Mutex::new(engine)),
                prefix: prefix.to_vec(),
                write_rx,
                read_rx,
            },
            VirtualHandle {
                write_tx,
                read_tx,
            },
        )
    }

    /// Serve forever: write pump (frame → one engine commit) + read pump
    /// (frame → engine op → response). The reference transport is plain
    /// threads; an async receiver would task-spawn the same loops. The
    /// two pumps share the engine behind the mutex; the channel receivers
    /// move into their threads (mpsc Receiver is not Sync — each pump
    /// owns its end exclusively, which is the correct shape anyway).
    pub fn serve(self) {
        let engine = self.engine.clone();
        let prefix = self.prefix.clone();
        let write_rx = self.write_rx;
        std::thread::spawn(move || {
            for bytes in write_rx {
                let Some(frame) = WriteFrame::decode(&bytes) else {
                    continue; // malformed frame = drop; garbage in, nothing stored
                };
                let mut engine = engine.lock().expect("engine lock");
                let mut batch = MemBatch::default();
                for (tag, key, value) in frame.0 {
                    match tag {
                        OP_PUT => batch.put(hosted_key(&prefix, &key), value),
                        OP_DELETE => batch.del(&hosted_key(&prefix, &key)),
                        // Reads never arrive on the write channel.
                        _ => {}
                    }
                }
                // One commit over the whole frame — the WAL boundary the
                // sender's batch maps onto (ADR-0010 §2). Fire-and-forget:
                // no return path, matching channel semantics.
                let _ = engine.commit_batch(batch);
            }
        });
        let engine = self.engine;
        let prefix = self.prefix;
        let read_rx = self.read_rx;
        std::thread::spawn(move || {
            for (reply_tx, bytes) in read_rx {
                let Some(frame) = ReadFrame::decode(&bytes) else {
                    continue;
                };
                let engine = engine.lock().expect("engine lock");
                let resp = match frame {
                    ReadFrame::Get { key } => ReadResponse {
                        value: engine.get(&hosted_key(&prefix, &key)),
                        ..Default::default()
                    },
                    ReadFrame::Scan { prefix: p } => ReadResponse {
                        suffixes: engine.scan_suffix(&hosted_key(&prefix, &p)),
                        ..Default::default()
                    },
                };
                if reply_tx.send(resp).is_err() {
                    break; // sender gone; this read pump's endpoints are dead
                }
            }
        });
    }
}

/// Sender key bytes → receiver-side physical key: pure concatenation,
/// receiver bytes first, sender bytes after, order never adjusted
/// (ADR-0010 §5). The receiver never parses what follows its prefix.
fn hosted_key(prefix: &[u8], sender_key: &[u8]) -> Vec<u8> {
    let mut full = Vec::with_capacity(prefix.len() + sender_key.len());
    full.extend_from_slice(prefix);
    full.extend_from_slice(sender_key);
    full
}
