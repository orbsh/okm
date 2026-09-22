# ADR-0021: Streaming scan over the wire — the remote path joins the lazy contract

Date: 2026-09-22
Status: Draft (design proposal; implementation lands after the frame-shape review).

## Context

ADR-0020 made lazy iteration the streaming contract of `VirtualStorage::scan_range_iter`: every local engine wraps its native owned iterator, and consumers that stop early (LIMIT, first match) pay only the reads performed. One backend was exempted: the remote path. `RemoteStore::scan_range_iter` buffers — it rides the same `OP_SCAN` frame, whose `OpResponse` carries the whole answer in one message. ADR-0020 recorded this as future work, not an obligation. It is now scheduled: the buffering is not a hypothetical cost, it is the exact degradation ADR-0020 removed everywhere else — a remote consumer asking for "the first 10 of a million" pays all million over the wire before seeing one.

### The constraint set (what any solution must preserve)

These are the properties the wire contract already locks (ADR-0010); a streaming design that breaks any of them is rejected by construction:

1. **Frames are opaque payloads; the transport never parses them.** Any chunking, acknowledgment, or cursor mechanics must live inside okm-wire's frame grammar — the WS/UDS/mpsc adapters stay "one `Vec<u8>` in, one `Vec<u8>` out".
2. **The executor surface is ONE method**: `NestStorage::apply(frame) -> Option<OpResponse>`. A streaming design may not require the host to hold per-consumer state between `apply` calls — `apply` is stateless by contract (the receiver hosts an engine, not sessions; ADR-0010 §7).
3. **Read correlation lives on the sender side of the transport envelope**, never in frame bytes (ADR-0010 §6). The mpsc reference transport carries `(reply_tx, frame)`; WS adapters correlate by envelope reply-to. The frame format itself has no request id.
4. **Zero OKM semantics in the wire**: keys/values are opaque bytes; the codec knows op tags and length buckets, nothing else.
5. **Write batching and order are already solved**: mutating ops in one frame commit together; scan runs after them (reads see prior same-frame writes). A streaming redesign must not reorder or split that guarantee.

### What exactly must change today

`OpResponse` has two fields — `value: Option<Vec<u8>>` (get) and `suffixes: Vec<Vec<u8>>` (scan). Both are whole-answer shapes. The scan answer also loses the values: `scan_range` on the wire returns key suffixes only, and the sender's `scan_range_iter` re-fetches each value with a per-key `OP_GET` round trip (`nest.rs`'s buffered path does `scan_range` + N × `get`). So the remote lazy path today is not just un-streamed, it is N+1 round trips on top.

## Decision

**One new op tag — `OP_SCAN_STREAM` (tag 4, first use of the reserved tag space) — and a chunked response grammar reusing `OpResponse`'s encoding, not a second response type.**

### Request: `OP_SCAN_STREAM` (one op, one frame)

Same op shape as `OP_SCAN`: key segment = full-key begin bound (hosted-prefix applied receiver-side, identical to today); value segment = the existing `[flag][end]` encoding of ADR-0020 (`0x00` unbounded, `0x01` + end bytes), extended by one trailing field:

```text
value segment: [flag u8][end bytes?][page u8]
page: requested batch size in ENTRIES (not bytes) —
      0 = engine default (256), 0xFF = "one giant page" (the old buffered shape)
```

Entries, not bytes, because the receiver must not parse keys (constraint 4) — a byte-budget page would require size-guessing anyway, and entries map 1:1 to what the sender's iterator yields. The page size is a hint, not a contract: the receiver may return fewer entries (it does when the range ends) and never more.

### Response: chunk = today's `OpResponse`, plus a 1-byte trailer

A stream is a sequence of `apply`-shaped responses over the SAME correlation channel the transport already provides. Each chunk:

```text
[has_value u8][value LK][value?] [count len-enc] per hit: [len][key][len][value?] [tail u8]
tail: 0x00 = more chunks follow (this chunk's count == page size)
      0x01 = final chunk (count may be anything, including 0)
```

Three properties fall out of reusing the response grammar:

- **Chunks are self-contained `OpResponse`s plus one byte.** A receiver that does not implement `OP_SCAN_STREAM` rejects the op (unknown tag = malformed frame, `apply` → `None`) and the sender falls back to the buffered path — backward compatibility without version negotiation.
- **The chunk carries values**, fixing the N+1 problem without a separate decision: each hit is `[len][key][len][value]`, mirroring the engine's `(key, value)` iterator item. The old `suffixes`-only answer stays for `OP_SCAN`; `OP_SCAN_STREAM` never strips values because its contract is the iterator's contract.
- **No cursor state on the receiver.** The sender owns resumption: each chunk's last key IS the cursor, and the next request is an ordinary `OP_SCAN_STREAM` with `begin = last_key + prefix_end-style increment` (exclusive begin via a `[0x02]` flag byte: exclusive-begin range). The receiver answers from whatever engine state exists now — no snapshot promise. This is the honest contract for a stateless executor (constraint 2): the remote stream is a sequence of consistent-at-chunk-time reads, not a frozen snapshot like fjall's local `Iter`. ADR-0020's semantics degrade exactly where the architecture already says they must.

### Sender shape: `RemoteStore::scan_range_iter` becomes genuinely lazy

```rust
// pseudo-structure of the sender adapter
fn scan_range_iter(&self, begin, end) -> ScanIter {
    ScanIter::Remote(RemoteScanIter {
        exec_tx, begin, end,
        page: 256,
        buf: VecDeque::new(),   // drained chunk
        done: false,
    })
}
// next(): pop buf; when empty and !done, request the next page
// (begin = last key + exclusive-begin flag), decode chunk, set done from tail byte.
// next_back(): NOT overridden — Remote arm degrades to buffered, same rule
// as slatedb's arm (ADR-0020: laziness lost where the engine cannot give
// it back; a backwards remote walk would need a descending-page request
// shape — deferred with the same reasoning).
```

The trait's return type does not change (`ScanIter` gains a `Remote` arm behind the existing enum — the object-safety workaround from ADR-0020 absorbs the new backend exactly as designed: adding an arm is an okm-core-internal event).

### What deliberately does NOT change

- `OP_SCAN` stays as-is: `scan`/`scan_suffix` consumers are equality-shaped, the buffered answer is their natural shape, and keeping them off the streaming path means the old grammar has one fewer moving part.
- `OP_GET` stays unary: a point lookup IS one item.
- The `apply` signature stays `Option<OpResponse>` — but it now answers a *page*, not a range. The host holds nothing between calls; the "stream" is the sender stitching pages together.
- No frame-level request ids, no acknowledgment frames, no receiver-side cursors: correlation and retry stay where ADR-0010 §6 put them (the transport envelope and the sender).

### Alternatives rejected

- **A frame-level stream protocol (SEQ/ACK frames, receiver-side cursor id).** Violates constraint 2 (stateful receiver) and constraint 3 (correlation in frame bytes). The transport already orders and correlates; restating it in the wire is the same fact stored twice, and it would couple the codec to session-oriented transports — the UDS-datagram / fire-and-forget shapes stop working.
- **One response type, streamed as raw byte spans with offsets.** Makes chunks non-self-contained: a chunk could only be parsed with its predecessors' context, so a dropped chunk poisons the rest. Self-contained chunks cost one repeated field and buy resumability for free.
- **Descending-page request for `next_back` (`[0x03]` reverse flag).** Technically symmetric, but no current consumer walks a remote range backwards without first having read forward; deferring keeps the flag byte space honest (flags are added when a use case exists, not for symmetry — same reasoning as ADR-0016's slot table).

## Consequences

- **The remote lazy path loses its N+1 penalty and its buffering penalty at once**: values ride the chunk; pages bound per-round-trip cost; early abandonment stops pulling pages.
- **Consistency semantics are explicit and weaker than local**: no snapshot across chunks. The local engines give snapshot iteration (fjall's nonce, redb's txn guard); the remote path gives per-chunk consistency. This must be documented on `RemoteStore::scan_range_iter` — a consumer needing a frozen view of a range across a remote link should materialize it (a Vec) and accept the cost.
- **okm-wire gains one op tag and one trailer byte — its zero-dependency, zero-semantics charter holds.** The hex tests extend to cover: chunk round trip, exclusive-begin flag, empty final chunk, unknown-tag fallback.
- **The buffered `OP_SCAN` path remains the compatibility floor** — a peer that predates tag 4 interoperates forever; the sender discovers the capability per-call (the fallback is invisible above the trait boundary).
- **`0xFF = one giant page` preserves the old behavior exactly** for consumers that want the buffered shape (the `scan_covered`-style internal callers), so nothing regresses while nothing is forced.
