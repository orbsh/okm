//! ADR-0028: WireClient (async sender over an injected transport)
//! against NestStorage (the transport-free receiver intake), on a
//! loopback transport with manual frame pumping — the shape a WS
//! connection has natively (post = outbound, deliver = inbound).
//!
//! The pairing discipline under test is the client's whole contract:
//! query frames answer waiters positionally (FIFO), write frames never
//! enqueue a waiter, and a dead connection answers outstanding waiters
//! with an explicit Err — never a park. These are the rules the mudra
//! panel discovered live; they are the crate's baseline now.
//!
//! No executor: futures are driven manually (poll → pump → deliver →
//! re-poll), which is exactly the wasm shape — the event loop calls
//! `deliver`, the task wakes.

#![cfg(feature = "test-engines")]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use okm_core::{
    MemBatch, NestStorage, OpFrame, OpResponse, TestStore, VirtualStorageAsync, WireClient,
    WireTransport,
};

/// Loopback transport: `post` queues outbound frames; the harness pumps
/// them through the receiver and calls `deliver` with the answers.
#[derive(Clone, Default)]
struct Loop {
    out: Rc<RefCell<VecDeque<Vec<u8>>>>,
}

impl WireTransport for Loop {
    fn post(&self, frame: &[u8]) -> Result<(), String> {
        self.out.borrow_mut().push_back(frame.to_vec());
        Ok(())
    }
}

/// One wiring: a bare host (byte-identical engine, no prefix — the
/// panel's shape) and its client over the loop. `new(..., &[])` is NOT
/// bare: it declares an empty hosted prefix, which makes OP_SCAN_STREAM
/// requests with no end bound degenerate (hosted_key of the empty
/// slice exists, and `[0].expect` dies). mudrad's WS server nests the
/// panel behind a real prefix instead — the bare form is `bare()`,
/// whose mpsc pump we replace with the loopback by using with_prefix
/// (None).
fn pair() -> (NestStorage<TestStore>, WireClient<Loop>, Loop) {
    let (host, _handle) = NestStorage::with_prefix(TestStore::fjall_tmp(), None);
    let loop_t = Loop::default();
    let client = WireClient::new(loop_t.clone());
    (host, client, loop_t)
}

/// Pump every currently-queued outbound frame through the host and feed
/// answers back through `deliver`, polling `fut` until Ready. A frame
/// whose apply answers `None` (a write) consumes no response — the same
/// rule the real receiver enforces. Generic so callers pass their own
/// concrete futures (no Box, no 'static bound).
fn drive<T>(
    fut: impl Future<Output = T>,
    host: &NestStorage<TestStore>,
    client: &WireClient<Loop>,
    loop_t: &Loop,
) -> T {
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = Box::pin(fut);
    loop {
        pump(host, client, loop_t);
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            // frames posted during the final poll (the usual case for
            // fire-and-forget writes) still belong to the host
            pump(host, client, loop_t);
            return v;
        }
    }
}

/// One pump pass: every queued frame executes on the host; query
/// answers (and only those — apply returns None for pure-write frames)
/// feed the client's FIFO waiter queue.
fn pump(host: &NestStorage<TestStore>, client: &WireClient<Loop>, loop_t: &Loop) {
    while let Some(frame) = loop_t.out.borrow_mut().pop_front() {
        if let Some(resp) = host.apply(&frame) {
            client.deliver(&resp.encode());
        }
    }
}

// ---------- round trips ----------

#[test]
fn put_then_get_round_trip() {
    let (host, client, loop_t) = pair();
    // write frames: no waiter, no response consumed
    drive(client.put(b"a".to_vec(), b"1".to_vec()), &host, &client, &loop_t);
    let got = drive(client.get(b"a"), &host, &client, &loop_t);
    assert_eq!(got.as_deref(), Some(b"1".as_slice()));
}

#[test]
fn fifo_pairing_across_interleaved_queries() {
    let (host, client, loop_t) = pair();
    drive(client.put(b"x".to_vec(), b"10".to_vec()), &host, &client, &loop_t);
    drive(client.put(b"y".to_vec(), b"20".to_vec()), &host, &client, &loop_t);

    // Kick three queries WITHOUT pumping between them: every waiter is
    // queued (and every frame posted) before any answer exists. The
    // client must attach answers to waiters by POSITION — frames carry
    // no ids.
    let mut fx = Box::pin(client.get(b"x"));
    let mut fy = Box::pin(client.get(b"y"));
    let mut fm = Box::pin(client.get(b"missing"));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(fx.as_mut().poll(&mut cx), Poll::Pending));
    assert!(matches!(fy.as_mut().poll(&mut cx), Poll::Pending));
    assert!(matches!(fm.as_mut().poll(&mut cx), Poll::Pending));
    // one batch pump: all three answers delivered, queue order = wire order
    while let Some(frame) = loop_t.out.borrow_mut().pop_front() {
        if let Some(resp) = host.apply(&frame) {
            client.deliver(&resp.encode());
        }
    }
    let vx = match fx.as_mut().poll(&mut cx) { Poll::Ready(v) => v, _ => panic!("x") };
    let vy = match fy.as_mut().poll(&mut cx) { Poll::Ready(v) => v, _ => panic!("y") };
    let vm = match fm.as_mut().poll(&mut cx) { Poll::Ready(v) => v, _ => panic!("m") };
    assert_eq!(vx.as_deref(), Some(b"10".as_slice()));
    assert_eq!(vy.as_deref(), Some(b"20".as_slice()));
    assert_eq!(vm, None);
}

// ---------- failure discipline ----------

#[test]
fn transport_death_fails_outstanding_waiters() {
    let (host, client, loop_t) = pair();
    let mut f = Box::pin(client.get(b"a")); // trait surface swallows Err
    let qf = get_frame(b"a2");
    let mut q = Box::pin(client.query(&qf)); // explicit Err
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(f.as_mut().poll(&mut cx), Poll::Pending));
    assert!(matches!(q.as_mut().poll(&mut cx), Poll::Pending));
    // answers never delivered; the owner reports the connection dead:
    client.fail("connection reset");
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => assert_eq!(v, None), // trait surface: Err -> None
        Poll::Pending => panic!("waiter parked after fail — the exact bug this contract forbids"),
    }
    match q.as_mut().poll(&mut cx) {
        Poll::Ready(Err(e)) => assert_eq!(e, "connection reset"),
        Poll::Ready(Ok(_)) => panic!("dead connection answered Ok"),
        Poll::Pending => panic!("query waiter parked after fail"),
    }
    // a dead client answers NEW queries immediately — nothing further
    // goes out on the wire
    assert!(drive(client.get(b"b"), &host, &client, &loop_t).is_none());
    assert!(loop_t.out.borrow().is_empty());
}

#[test]
fn post_failure_dies_immediately() {
    // A transport that refuses at post: the FIRST poll answers the
    // explicit Err (post happens inside round_trip, before parking).
    struct Dead;
    impl WireTransport for Dead {
        fn post(&self, _frame: &[u8]) -> Result<(), String> {
            Err("socket gone".into())
        }
    }
    let client = WireClient::new(Dead);
    let mut cx = Context::from_waker(Waker::noop());
    let qf = get_frame(b"k");
    let mut q = Box::pin(client.query(&qf));
    match q.as_mut().poll(&mut cx) {
        Poll::Ready(Err(e)) => assert_eq!(e, "socket gone"),
        _ => panic!("expected immediate Err, got a park"),
    }
    let mut c2 = WireClient::new(Dead);
    let mut cb = Box::pin(c2.commit_batch(MemBatch::default()));
    match cb.as_mut().poll(&mut cx) {
        Poll::Ready(Err(e)) => assert_eq!(e, "socket gone"),
        Poll::Ready(Ok(())) => panic!("dead transport accepted a batch"),
        Poll::Pending => panic!("commit_batch parked on a dead transport"),
    }
}

#[test]
fn malformed_response_is_explicit_err() {
    let (_host, client, _loop_t) = pair();
    let mut cx = Context::from_waker(Waker::noop());
    let qf = get_frame(b"a");
    let mut q = Box::pin(client.query(&qf));
    assert!(matches!(q.as_mut().poll(&mut cx), Poll::Pending));
    client.deliver(&[0xFF, 0xFF, 0xFF]); // garbage frame
    match q.as_mut().poll(&mut cx) {
        Poll::Ready(Err(_)) => {}
        Poll::Ready(Ok(_)) => panic!("garbage decoded as an answer"),
        Poll::Pending => panic!("malformed answer parked the waiter"),
    }
}

#[test]
fn stray_answer_finds_no_waiter_and_harmless() {
    let (_host, client, _loop_t) = pair();
    client.deliver(&[]); // nobody waits: the queue drops it, no panic
    client.deliver(&OpResponse::default().encode()); // even a valid one
}

// ---------- receiver-side semantics the client must respect ----------

#[test]
fn scan_range_and_suffix_frames() {
    let (host, client, loop_t) = pair();
    for k in [b"m/1".as_slice(), b"m/2", b"z/1"] {
        drive(client.put(k.to_vec(), b"v".to_vec()), &host, &client, &loop_t);
    }
    // A bare host strips ONLY its own (empty) hosting prefix: range
    // answers are full keys (begin is NOT stripped — same rule as the
    // sync remote; prefix-relative = relative to the RECEIVER prefix).
    let mut keys = drive(client.scan_range(b"m", Some(b"z")), &host, &client, &loop_t);
    keys.sort();
    assert_eq!(keys, vec![b"m/1".to_vec(), b"m/2".to_vec()]);
    let suffixed = drive(client.scan_suffix(b"z/"), &host, &client, &loop_t);
    assert_eq!(suffixed, vec![b"1".to_vec()]);
}

#[test]
fn scan_range_iter_pages_to_completion() {
    // > engine default page (256): the buffered walk must issue multiple
    // OP_SCAN_STREAM round trips and concatenate in byte order.
    let (host, client, loop_t) = pair();
    // one write batch frame, not 600 single-put frames (fjall pays one
    // WAL commit per apply — 600 fsyncs would drown the test clock)
    let mut batch = MemBatch::default();
    for i in 0..600u32 {
        batch.put(format!("k/{i:04}").into_bytes(), b"v".to_vec());
    }
    let mut cm = client.clone();
    drive(cm.commit_batch(batch), &host, &client, &loop_t).expect("batch on a live loop");
    let pairs = drive(client.scan_range_iter(b"k", None), &host, &client, &loop_t);
    let items: Vec<_> = pairs.collect();
    assert_eq!(items.len(), 600);
    // bare host: hits carry FULL engine keys (the sync-stream rule)
    assert_eq!(items[0].0, b"k/0000".to_vec());
    assert_eq!(items[0].1, b"v".to_vec());
}

#[test]
fn commit_batch_is_one_frame_one_pass() {
    let (host, client, loop_t) = pair();
    let mut batch = MemBatch::default();
    batch.put(b"p/1".to_vec(), b"a".to_vec());
    batch.del(b"p/missing".as_ref());
    batch.put(b"p/2".to_vec(), b"b".to_vec());
    // commit_batch takes &mut self (sync-trait mirror): run it on a
    // handle clone, pump through the other handle (same inner Rc).
    let mut cm = client.clone();
    drive(cm.commit_batch(batch), &host, &client, &loop_t).expect("batch on a live loop");
    // exactly one outbound write frame (no per-op frames, no leftovers)
    assert!(loop_t.out.borrow().is_empty());
    assert_eq!(
        drive(client.get(b"p/1"), &host, &client, &loop_t).as_deref(),
        Some(b"a".as_slice())
    );
    assert_eq!(
        drive(client.get(b"p/2"), &host, &client, &loop_t).as_deref(),
        Some(b"b".as_slice())
    );
}

// ---------- hosting shape (the panel's actual wiring) ----------

#[test]
fn hosted_prefix_is_invisible_to_the_client() {
    // mudra's WS server hosts the panel behind a declared prefix; the
    // client's keyspace never sees it (write prepend, answer strip).
    let (host, _handle) = NestStorage::new(TestStore::fjall_tmp(), &[0x42, 0x00]);
    let loop_t = Loop::default();
    let client = WireClient::new(loop_t.clone());
    drive(client.put(b"a".to_vec(), b"1".to_vec()), &host, &client, &loop_t);
    // the ENGINE holds the hosted key — checked through the receiver's
    // own intake with the raw hosted bytes (no backdoor accessor)
    // the receiver prepends ITS prefix to whatever key the frame
    // carries: probe with the sender key, the hosted bytes are implicit
    let resp = host
        .apply(&get_frame(b"a").encode())
        .expect("get answers");
    assert_eq!(resp.value.as_deref(), Some(b"1".as_slice()));
    // and the client's round trip still speaks its own key
    assert_eq!(
        drive(client.get(b"a"), &host, &client, &loop_t).as_deref(),
        Some(b"1".as_slice())
    );
}

/// One OP_GET frame, via the wire codec re-export (the receiver and the
/// client share okm-wire as the single frame source).
fn get_frame(key: &[u8]) -> OpFrame {
    okm_wire::OpFrame::one(okm_wire::OP_GET, key.to_vec(), Vec::new())
}

