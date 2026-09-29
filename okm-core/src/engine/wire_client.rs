//! Async wire sender (ADR-0028): [`WireClient`] rides ANY byte transport
//! that carries one `Vec<u8>` per frame and delivers one response frame
//! per query frame, in send order. The transport is injected — the
//! client never learns whether it is a WebSocket message, a UDS
//! datagram, or an in-process pump; the receiver-side intake is already
//! transport-free (`NestStorage::apply`). This module makes the sender
//! side symmetric.
//!
//! Correlation model (inherited from ADR-0010 §6, made byte-visible by
//! the absence of any request id):
//! - QUERY frames (get/scan/scan-stream) enqueue exactly one waiter;
//!   the client answers waiters IN FIFO order. Legal because the
//!   receiver applies a connection's frames sequentially (answer order
//!   = send order) and mutating frames never enqueue a waiter (the
//!   write answer is the transport's delivery, not the bytes).
//! - WRITE frames are fire-and-forget: no waiter, no response consumed.
//! - Transport failure surfaces as explicit `Err` to every outstanding
//!   waiter — a hung await is the failure mode the mudra panel paid
//!   for; never reproduce it.
//!
//! One client per CONNECTION (FIFO pairing is per-connection), and no
//! re-entrancy across `post` and `deliver` on the same client (the
//! caller pumps responses from the transport's event loop, not from
//! inside a client method).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use okm_wire::{
    OP_DELETE, OP_GET, OP_PUT, OP_SCAN, OP_SCAN_STREAM, OpFrame, OpResponse, TAIL_MORE,
};

use crate::engine::storage::{MemBatch, ScanIter, VirtualStorageAsync};

/// The injected transport: hand it one request frame, it carries it to
/// the receiver. Returning `Err` means the connection is gone — the
/// client fails all outstanding waiters immediately.
pub trait WireTransport {
    fn post(&self, frame: &[u8]) -> Result<(), String>;
}

/// A waiter for one query response: `Some` = answered, `None` = the
/// connection died before the answer arrived (explicit failure, never a
/// silent park).
struct Slot {
    state: Option<Result<OpResponse, String>>,
    waker: Option<Waker>,
}

impl Slot {
    fn resolve(&mut self, r: Result<OpResponse, String>) {
        self.state = Some(r);
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

/// The future behind one query call. Polls the shared slot; the slot is
/// resolved by `deliver` (FIFO) or by `fail_all` (transport death).
struct SlotFuture {
    slot: Rc<RefCell<Slot>>,
}

impl Future for SlotFuture {
    type Output = Result<OpResponse, String>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut s = self.slot.borrow_mut();
        if let Some(r) = s.state.take() {
            Poll::Ready(r)
        } else {
            s.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Sender-side stream state (ADR-0021 mirrored to the async world):
/// drained page buffer, sender-owned cursor (last consumed key, re-sent
/// verbatim as the next page's exclusive begin), end flag from the
/// chunk tail byte. State lives on the SENDER — the receiver is
/// stateless by contract.
struct PageState {
    begin: Vec<u8>,
    end: Option<Vec<u8>>,
    page: u8, // 0 = engine default
    buf: VecDeque<(Vec<u8>, Vec<u8>)>,
    cursor: Option<Vec<u8>>,
    done: bool,
}

/// The client: transport + FIFO waiter queue. `Clone` is cheap (shared
/// inner state via `Rc`) — several call sites may issue queries on one
/// connection; FIFO order is the wire's order.
#[derive(Clone)]
pub struct WireClient<T: WireTransport> {
    inner: Rc<RefCell<Inner<T>>>,
}

struct Inner<T: WireTransport> {
    transport: T,
    waiters: VecDeque<(Rc<RefCell<Slot>>, bool)>,
    /// Set by `fail_all`; new queries then answer the explicit Err
    /// instead of enqueuing onto a dead connection.
    dead: Option<String>,
}

impl<T: WireTransport> WireClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Inner {
                transport,
                waiters: VecDeque::new(),
                dead: None,
            })),
        }
    }

    /// A POST failed (or the owner learned the connection died): the
    /// dead reason answers every outstanding waiter with `Err` and all
    /// later queries too. Called by the transport's error path, not by
    /// the client's own methods (post is `&self` and cannot retry).
    pub fn fail(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let waiters: Vec<_> = self
            .inner
            .borrow_mut()
            .waiters
            .drain(..)
            .collect();
        for (slot, _) in waiters {
            slot.borrow_mut().resolve(Err(reason.clone()));
        }
        self.inner.borrow_mut().dead = Some(reason);
    }

    /// The inbound pump: one response frame arrived on the connection.
    /// Answers the OLDEST outstanding waiter — no id, correlation is
    /// positional (the receiver applies this connection's frames in
    /// order; write frames never enqueued a waiter).
    pub fn deliver(&self, bytes: &[u8]) {
        let (slot, chunk) = {
            let mut inner = self.inner.borrow_mut();
            match inner.waiters.pop_front() {
                Some(s) => s,
                // An answer nobody waits for: a post-delivery after
                // `fail` drained the queue, or a spurious write answer.
                // Nothing to resolve — the bytes are the transport's
                // to lose, the client's contract is the waiter queue.
                None => return,
            }
        };
        // The waiter's recorded op picks the decode shape — same rule
        // as the sync RemoteStore's stream_page ("the sender knows
        // which op it issued"): never sniffed from bytes.
        let resp = if chunk {
            OpResponse::decode_chunk(bytes)
        } else {
            OpResponse::decode(bytes)
        }
        .map(Ok)
        .unwrap_or_else(|| Err("wire client: malformed response frame".to_string()));
        slot.borrow_mut().resolve(resp);
    }

    fn round_trip(&self, frame: &OpFrame) -> SlotFuture {
        let slot = Rc::new(RefCell::new(Slot {
            state: None,
            waker: None,
        }));
        // The answer shape is known POSITIONALLY: the waiter records the
        // op it is waiting for (OP_SCAN_STREAM answers are chunks —
        // `OpResponse::decode_chunk`, never sniffed from bytes).
        let chunk = frame.0.iter().any(|(tag, _, _)| *tag == OP_SCAN_STREAM);
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(reason) = inner.dead.clone() {
                drop(inner);
                slot.borrow_mut().resolve(Err(reason));
                return SlotFuture { slot };
            }
            let post = inner.transport.post(&frame.encode());
            if let Err(e) = post {
                inner.dead = Some(e.clone());
                // Fail the queue AND this request; the just-posted
                // frame may or may not have left, nothing may follow it.
                let stale: Vec<_> = inner.waiters.drain(..).collect();
                for (s, _) in stale {
                    s.borrow_mut().resolve(Err(e.clone()));
                }
                slot.borrow_mut().resolve(Err(e));
                return SlotFuture { slot };
            }
            // Enqueue AFTER a successful post, in send order: the FIFO
            // queue position mirrors the wire position.
            inner.waiters.push_back((Rc::clone(&slot), chunk));
        }
        SlotFuture { slot }
    }

    async fn ask(&self, frame: &OpFrame) -> Result<OpResponse, String> {
        self.round_trip(frame).await
    }

    /// The explicit-error surface: the trait methods above swallow
    /// failures into the empty shapes their signatures allow (`None` /
    /// `[]`). A wasm event loop that wants to DISTINGUISH "absent" from
    /// "connection died" — to reconnect, to park writes — calls this.
    /// Same FIFO discipline; the Err is the transport failure reason.
    pub fn query(&self, frame: &OpFrame) -> impl Future<Output = Result<OpResponse, String>> + Unpin {
        self.round_trip(frame)
    }

    fn page_request(begin: &[u8], end: Option<&[u8]>, exclusive: bool, page: u8) -> OpFrame {
        let mut value = vec![match (exclusive, end.is_some()) {
            (true, true) => 0x02,
            (true, false) => 0x03, // unbounded + exclusive (ADR-0021
            (false, true) => 0x01, // closure: cursor resume of an
            (false, false) => 0x00, // unbounded stream
        }];
        if let Some(end) = end {
            value.extend_from_slice(end);
        }
        value.push(page);
        OpFrame::one(OP_SCAN_STREAM, begin.to_vec(), value)
    }

    /// Pull pages until the stream is final; the drained buffer is the
    /// (key, value) materialization behind `scan_range_iter` (the async
    /// trait hands back a SYNC `ScanIter`, so laziness across the await
    /// boundary is structurally out — buffered walk, values ride the
    /// chunk, one OP_SCAN_STREAM round trip per page, no N+1).
    async fn refill(&self, st: &mut PageState) -> Result<(), String> {
        if st.done {
            return Ok(());
        }
        let (exclusive, begin) = match st.cursor.take() {
            Some(last) => (true, last),
            None => (false, std::mem::take(&mut st.begin)),
        };
        let resp = self
            .ask(&Self::page_request(
                &begin,
                st.end.as_deref(),
                exclusive,
                st.page,
            ))
            .await?;
        debug_assert!(resp.tail.is_some(), "stream answer is a chunk");
        st.done = resp.tail != Some(TAIL_MORE);
        st.buf.extend(resp.hits);
        // The eager walk never CONSUMES the buffer, so the cursor must
        // advance here — the last key of the latest chunk. (The sync
        // RemoteScanIter advances on consume_front instead; same effect:
        // every buffered key was "consumed" by the materialization.)
        st.cursor = st.buf.back().map(|(k, _)| k.clone());
        Ok(())
    }
}

impl<T: WireTransport> VirtualStorageAsync for WireClient<T> {
    async fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        // fire-and-forget: one frame, no waiter (ADR-0010 §6 — the
        // delivery guarantee is the transport's, never the frame's).
        let inner = Rc::clone(&self.inner);
        let inner = inner.borrow_mut();
        if inner.dead.is_some() {
            return; // writes on a dead connection are lost by contract
        }
        let _ = inner
            .transport
            .post(&OpFrame::one(OP_PUT, key, value).encode());
    }

    async fn del(&self, key: &[u8]) {
        let inner = Rc::clone(&self.inner);
        let inner = inner.borrow_mut();
        if inner.dead.is_some() {
            return;
        }
        let _ = inner
            .transport
            .post(&OpFrame::one(OP_DELETE, key.to_vec(), Vec::new()).encode());
    }

    async fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.ask(&OpFrame::one(OP_GET, key.to_vec(), Vec::new()))
            .await
            .ok()
            .and_then(|r| r.value)
    }

    async fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // legacy prefix-scan shape: empty value segment
        self.ask(&OpFrame::one(OP_SCAN, prefix.to_vec(), Vec::new()))
            .await
            .ok()
            .map(|r| r.suffixes)
            .unwrap_or_default()
    }

    async fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        let mut value = vec![0u8];
        if let Some(end) = end {
            value[0] = 1;
            value.extend_from_slice(end);
        }
        self.ask(&OpFrame::one(OP_SCAN, begin.to_vec(), value))
            .await
            .ok()
            .map(|r| r.suffixes)
            .unwrap_or_default()
    }

    /// Buffered page walk (ADR-0028 boundary): the async trait must hand
    /// back the SYNC `ScanIter` and a sync `next` cannot await a refill,
    /// so the wire's lazy pages are MATERIALIZED here — one
    /// `OP_SCAN_STREAM` page per round trip (values ride the chunk, no
    /// N+1), drained until the tail byte says final. Early abandonment
    /// across the await boundary is the future wire-surface question;
    /// eager materialization is the documented today-shape.
    async fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> ScanIter {
        let mut st = PageState {
            begin: begin.to_vec(),
            end: end.map(|e| e.to_vec()),
            page: 0, // 0 = engine default (256)
            buf: VecDeque::new(),
            cursor: None,
            done: false,
        };
        // Any wire failure ends the walk where it stands — a truncated
        // stream beats a panic, and the `Vec` return loses the error
        // anyway (the trait surface has no Result; the same truncation
        // shape the sync remote's buffered default has).
        while !st.done {
            if self.refill(&mut st).await.is_err() {
                break;
            }
        }
        // ScanIter::Buffered speaks `vec::IntoIter`; collect once.
        ScanIter::Buffered(st.buf.into_iter().collect::<Vec<_>>().into_iter())
    }

    /// One `commit_batch` = one frame = one receiver execution pass —
    /// the cross-assembly-point atomic path (ADR-0003) survives
    /// remoteness and async transport alike.
    async fn commit_batch(&mut self, batch: MemBatch) -> Result<(), String> {
        let inner = Rc::clone(&self.inner);
        let inner = inner.borrow_mut();
        if let Some(reason) = inner.dead.clone() {
            return Err(reason);
        }
        inner
            .transport
            .post(&OpFrame::write_batch(&batch.ops).encode())
    }
}
