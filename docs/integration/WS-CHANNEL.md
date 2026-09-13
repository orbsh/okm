# Integrating with Existing WS Channels: Remote VirtualStorage over a Running Transport

> Sibling docs: [Extension types and primitives](EXTENSION-TYPES.md) —
> FTS/vector/graph recipes over OKM's primitives. This doc is the
> transport integration counterpart: how an application with an existing
> WebSocket connection (or any message-carrying channel) executes OKM
> operations on a remote engine without opening a second transport.

## The premise

OKM's remote backend (ADR-0010) already separates three concerns:

```text
sender (RemoteStore)      implements VirtualStorage; frames ops
codec (okm-wire)          hand-parsed frames: tag + lengths + raw bytes
receiver (StorageHost)    prepends its declared prefix, replays on the engine
```

The transport between sender and receiver is a backend-internal detail:
the codec defines what a frame IS, never how frames travel. A frame is a
`Vec<u8>`; anything that can carry one message per frame can be the
transport. This doc walks through the WS case end to end.

## The one rule: frames are opaque payloads of the channel

An existing WS connection usually has its own message protocol (JSON
envelopes, protobuf events, realm messages). The integration rule is
symmetric and strict:

- The OKM side must NOT parse the channel's envelope — it produces a
  frame's bytes and hands them over as one opaque payload.
- The channel side must NOT parse the OKM frame — it forwards the
  payload to a declared `StorageHost` and returns whatever comes back.

Two ways to satisfy this, depending on who owns the connection:

### Shape A: OKM inside the WS app (the usual case)

The application runs both the WS client and the `StorageHost`; remote
OKM instances elsewhere send operations **over this connection**.

```text
Krystallizer process                    Aura node process
┌─────────────────────┐                ┌──────────────────────────────┐
│ RemoteStore         │                │ WS server                    │
│   │ frame bytes     │   WebSocket    │   │ envelope → route:        │
│   ▼                 │ ─────────────► │   ▼                          │
│ okm-wire encode     │                │ StorageHost::serve pumps     │
└─────────────────────┘                │   ├─ [prefix] engine ops     │
                                       │   └─ replies                 │
                                       └──────────────────────────────┘
```

Receiver side (Aura): the WS handler receives an envelope, extracts the
OKM payload by envelope convention, and hands it to the host's intake.
The host needs its pumps addressable from the WS handler instead of
internal mpsc channels — a 20-line adapter:

```rust
pub struct WsIntake<S: VirtualStorage> {
    host: Arc<Mutex<StorageCore<S>>>,
}

impl<S: VirtualStorage + Send + 'static> WsIntake<S> {
    /// One WS message carrying a write frame. Returns nothing —
    /// fire-and-forget, exactly like the mpsc write pump.
    pub fn write_frame(&self, frame: &[u8]) {
        self.host.lock().unwrap().apply_write(frame);
    }
    /// One WS message carrying a read frame. Returns the response frame
    /// bytes — the WS handler sends them back over the same connection.
    pub fn read_frame(&self, frame: &[u8]) -> Option<Vec<u8>> {
        self.host.lock().unwrap().apply_read(frame)
    }
}
```

(`StorageCore` is the current `StorageHost` with its engine + prefix but
the pumps factored into `apply_write` / `apply_read` methods — the same
logic the mpsc pumps call. No behavior change; the mpsc wiring stays as
the reference transport.)

Sender side (Krystallizer): `RemoteStore` currently hard-wires
`mpsc::Sender<Vec<u8>>`. The generalization is to make the sender
generic over a tiny transport trait instead of the concrete channel:

```rust
pub trait FrameTransport {
    /// Ship one write frame (fire-and-forget).
    fn send_write(&self, frame: Vec<u8>);
    /// Ship one read frame and block for its response. Correlation is
    /// the transport's business (in-process mpsc pairs it implicitly;
    /// WS pairs it by the connection itself or an envelope request id).
    fn round_trip(&self, frame: Vec<u8>) -> Vec<u8>;
}
```

The WS implementation wraps the app's existing connection handle:

```rust
struct WsTransport<C: WsConn> {
    conn: C,                       // the EXISTING connection, shared
    write_topic: &'static str,     // envelope convention: where OKM
    read_topic: &'static str,      // frames live inside the protocol
}

impl<C: WsConn> FrameTransport for WsTransport<C> {
    fn send_write(&self, frame: Vec<u8>) {
        self.conn.send_json(topic(self.write_topic), frame);  // opaque payload
    }
    fn round_trip(&mut self, frame: Vec<u8>) -> Vec<u8> {
        let id = self.conn.next_request_id();                 // correlation id
        self.conn.send_json(request(self.read_topic, id), frame);
        self.conn.wait_reply(id)                              // blocks this task
    }
}
```

Note what does NOT change: the frame bytes, the codec, the host, the
prefix discipline, the batch atomicity mapping. Only the last mile
grows an envelope.

### Shape B: OKM as the WS app's storage service

The reverse embedding — the WS app is the storage client, OKM runs in
its own process (or the same process, different actor), reachable
through a topic. Identical to Shape A with the roles of the two
envelope sides swapped; the adapter and transport trait are the same
two pieces.

## Envelope conventions that work

The channel's protocol needs three slots for OKM traffic:

1. **Routing**: which declared host (which app/prefix) the frame is
   for — when one connection serves several instances. This is envelope
   data (a topic, a route key), never frame data: the frame itself does
   not know the receiver's prefix (ADR-0010 §5, knowledge asymmetry).
2. **Kind**: write (fire-and-forget) vs read (expects a reply). The
   frame's leading op tag could carry this, but the envelope knowing it
   lets the channel route replies without peeking into the frame.
3. **Correlation** (reads only): an id matching response to request.
   mpsc gets this for free by pairing channels; WS either dedicates a
   request/reply pattern or multiplexes by id. Either is transport
   policy — the frame stays silent about it.

What must NOT enter the envelope: layout versions, schema hints,
compression flags, anything that makes the receiver understand frame
content. The envelope treats OKM frames exactly as it treats any other
binary payload — that is the whole isolation mechanism.

## Ordering and atomicity guarantees over WS

- **Write order**: WS messages arrive in send order per connection; the
  host's write pump applies frames in arrival order. One frame = one
  engine `commit_batch`, so the sender's batch atomicity maps 1:1.
  Multiple connections to one host = no total order — route each
  sender's writes to its own host instance (one `#[kv_storage]` per
  application anyway) or accept interleaving (fine when senders touch
  disjoint key ranges).
- **Reads**: any connection can serve reads; the host's engine mutex
  serializes them against the write pump.
- **Reconnect**: a dropped WS loses in-flight frames (fire-and-forget
  writes have no ack). If the sender needs delivery guarantees, that is
  a channel-level concern (ack envelope + retry idempotency), not an
  OKM concern — OKM's engine ops are idempotent by key, so replaying a
  put is safe, replaying a delete is a no-op, but a put-delete-put
  sequence replayed from a checkpoint needs the checkpoint at the
  sender. Design this at the envelope layer when the requirement is
  real; do not preemptively build it.

## Why this stays out of okm-core

The WS adapter is transport glue: it binds an existing connection to
the host's intake and adds an envelope convention. Every application's
channel protocol differs, so the adapter cannot be a library — it is
per-integration code in the application (or the Aura node), built from
two pieces okm-core provides: `StorageHost`'s factored pump methods and
`RemoteStore`'s transport trait. The doc-level contract is the three
envelope slots above.

> 中文本篇：[经由既有 WS 通道集成](WS-CHANNEL.zh-CN.md)
