# ADR-0039: the storage trait's write verbs return `Result`

Status: Accepted (2026-10-24). Follow-up to ADR-0037 — that one gave the write
verbs an error channel and named the two surfaces it left closed; this one opens
the storage trait. The read-side half (corrupt bytes) stays closed on purpose.

## Context

ADR-0037 made every write verb fallible and recorded what it deliberately did
not touch: "the raw per-op storage methods (`VirtualStorage::put` / `del`) — both
are 'the store is not what it claims' failures … widening either is a different
change with a different subject (integrity reporting, storage trait)."

The storage trait is now the last place a caller's `Result` dead-ends. `get` and
every scan have answered `Result` since ADR-0038 ("a failed read is not an absent
row"), so a backend has a place to put a read refusal. `put` and `del` still
return `()`, so the same backend has nowhere to put a write refusal — and the
engines proved it: `slatedb_backend` answered its write errors with 6 panics and
`expect`s, `redb_backend` with 8, `fjall_backend` with 2, `localstorage_backend`
with 2, and the two senders (`nest`'s `RemoteStore`, `WireClient`) dropped a
write on a closed channel or a dead connection without a word.

The failure modes those sites hide are exactly the ones a caller CAN act on:

- the store refused the commit (I/O, a fenced writer, a full quota, a store
  closed for writes);
- the transport is gone, so this frame will never leave (`WireClient::fail`
  already records a `dead` reason for queries — writes ignored it).

Neither is "the store is not what it claims". A corrupt payload is that; a
refused write is a store saying no, and the writer is the party that must hear it.

## Decision

1. **`VirtualStorage::put` / `del` and their async twins return
   `crate::error::Result<()>`.** `Err` is `Error::Engine(text)` carrying the
   backend's own message. `Ok(())` from `put` means the store has the bytes (not
   just that the call was accepted); `Ok(())` from `del` also covers "there was
   nothing to delete" — absence is not a failure, the same rule `get` follows for
   `Ok(None)`.
2. **Every in-crate backend maps its native error**, no `expect` left on the
   write path: `redb` reuses its `eng` mapper (begin_write / open_table / insert /
   remove / commit), `fjall` maps `insert` / `remove`, `localStorage` maps
   `set_item` (a quota refusal is the browser saying no) / `remove_item`, and
   `slatedb`'s async impl awaits the durable handle so `Ok` means the WAL
   boundary was crossed — the same rule its `commit_batch` override already
   followed.
3. **The sync slatedb path becomes one round trip.** `Cmd::Put` / `Cmd::Del` gain
   a reply channel, so `put_sync` / `del_sync` answer the engine's verdict instead
   of posting into the void; the driver thread awaits durability and answers,
   rather than panicking on the driver thread where no caller could see it.
   Channel order already guaranteed a later read observes the write, so the price
   is one `recv`, not a scheduling change.
4. **`RemoteStore::send_write` returns `Result`**: a closed executor channel means
   the nest went away and the frame is lost. `commit_batch` propagates it instead
   of unwrapping.
5. **`WireClient` reports its own half.** A dead connection or a refused `post`
   is `Err` — before, both were silent drops, which made a lost write
   indistinguishable from a committed one. The delivery guarantee itself stays
   with the transport (ADR-0010 §6): the client reports that the frame left, not
   that a peer applied it.
6. **`BatchStore` stays infallible** (`Ok(())`): buffering an op into a `Batch`
   cannot fail; `commit_batch` is where the engine answers.
7. **`AsyncJunction::link` / `unlink` become fallible**, matching the sync
   `Junction` that ADR-0037 already made fallible.

## Alternatives rejected

- **Keep `()` plus a panic in the backend** (the state before). A server that
  writes through `put` dies where the same store error through `merge` is a
  value — ADR-0037's own argument, one layer down. The layer boundary does not
  change who can act on the error.
- **A deferred error slot the next `get` reports.** The write that failed would
  surface as another operation's error, attributed to the wrong caller; an
  embedding that writes and never reads again would never learn, and a batch
  would report one error for N ops with no way to say which.
- **`put` kept, plus a `try_put`.** Two verbs for one operation invites the
  silent path — the same shape ADR-0037 rejected as "half-Result".
- **Make the host-injected `Engine` trait in `bindings/` carry `okm_core::Error`.**
  That trait is an FFI contract that deliberately carries no okm types ("bytes in,
  bytes out", rev-independent): the host may hold a different okm build, so its
  failure type is not ours to name. It answers `Result<_, String>` instead (which
  is what landed), and the box that adapts it maps the string into `Error::Engine`.

## Consequences

- The write path of every backend can now refuse in type: 6 slatedb sites (async
  put/del, sync put_sync/del_sync, the driver's two arms), 8 in redb, 2 in
  `fjall`, 2 in `localStorage`, `nest`'s `send_write` and `commit_batch`, and the
  two wire senders.
- Sync slatedb writes now wait for durability and for the driver's answer. That is
  a latency change on the sync path — the correctness it buys is that `Ok` means
  committed, which the other sync engines (redb commit, fjall insert) already
  meant.
- `bindings/*` are NOT workspace members, so no workspace check or test sees them.
  The host-injected seam is `Result<_, String>` now (commit `38c5c6c`, ADR-0037 §1):
  okm's error type must not cross that boundary, a plain string can. What is still
  open there is the bindings' own bodies — 18 `unwrap`/`expect` in `okm-python`,
  10 in `okm-steel`, 1 in `okm-entry` — i.e. poison and host-call handling, not the
  storage contract.
- **The read arms went with it** (same round): the slatedb driver's `Get`,
  `ScanRange`, `ScanSuffix` and `ScanStream` answer `Result<_, String>` over the
  channel (`SlatedbSync::request` returns `crate::error::Result`, and a scan that
  cannot open hands back an iterator whose ONE item is the error), and redb's
  `scan_range_iter` reports a failed `begin_read`/`range` the same way — the item
  carries the fault, per ADR-0038, because a streaming constructor cannot answer
  for it. `localStorage` had no production read panic left. What remains in the
  engine layer is construction and invariants only: `SlatedbSync::open_mem`'s
  runtime/thread spawn (two `expect`s on a path that runs once), `nest`'s executor
  invariants (exec_rx lock, serve twice, engine lock, begin present), and
  `test_engine`'s open helpers.
- **A refused write comes back as `Err`, and tests lock it** (same round):
  `write_refusal_test::a_refused_write_comes_back_as_the_engines_text` drives a
  store that refuses through `Collection::put` and asserts the engine's own text,
  and `wire_client_test::a_write_on_a_dead_connection_answers_err` /
  `a_write_whose_post_fails_answers_err` cover the sender's half. Still NOT
  covered: fault injection into the native engines themselves (forcing redb /
  fjall / slatedb to refuse means corrupting a live store; their mapping is one
  `map_err` line with no branch to get wrong), and the nest's "executor gone" arm,
  which cannot fire while any sender is alive — it stays a defensive arm.
- **The default `commit_batch` is not atomic under a refusal.** It replays ops
  through `put`/`del`, so an engine that refuses op N leaves ops 1..N applied —
  only engines with a native batch (fjall, redb, slatedb) commit all-or-nothing.
  `a_replayed_batch_leaves_the_ops_before_the_refusal` locks that boundary. It is
  also why ADR-0037's "a refused write writes nothing" holds for *validation*
  refusals (they run before any op enters the batch) but not for engine refusals.
- **A write that panics inside the engine is still invisible to its sender.** The
  nest pump wraps each frame in `catch_unwind` and a writer carries no reply box
  (ADR-0010 §6), so an engine panic during a write looks like success. Making
  writes answer would cost the fire-and-forget shape — its own decision, queued in
  PLAN.md.
- Downstream migration is mechanical and is the reason this ships as one API
  break: `?` where the caller already returns a result, `.unwrap()` in tests
  (ADR-0037's migration rule).