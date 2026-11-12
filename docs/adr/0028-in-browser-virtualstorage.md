# ADR-0028: In-browser VirtualStorage family — localStorage engine + async wire client

> **Languages:** [English](0028-in-browser-virtualstorage.md) (primary) · [中文](0028-in-browser-virtualstorage.zh-CN.md)

**Status:** Implemented (8d00132 + 8a12f81). **Amended 2026-11-10: the
`LocalStorageStore` half is WITHDRAWN** — the sync browser engine, its `localstorage`
feature and `web-sys` left `okm-core`; the wire client half stands. See §Withdrawal.

## Context

mudra R3 (mudra `PLAN.md` §11) puts two storage roles inside the browser:

1. **panel-local UI state** (tree expansion, sort preference, drafts) — the
   typed layer (`Collection`/`Graph`) binds the SYNC `VirtualStorage`, so this
   role needs a browser storage that is genuinely synchronous. localStorage is
   the only one whose API is sync (`getItem`/`setItem`/`removeItem`);
   IndexedDB is async by birth and structurally does not fit the sync trait.
2. **remote business data** — the panel already hand-rolled a
   RemoteStore-over-WS (mudra `crates/mudra-panel/src/remote.rs`): one WS
   message = one `okm-wire` frame, query frames paired FIFO with responses,
   write frames fire-and-forget (corrected: writes answer an ack now, so a
   shared client pairs every frame — see ADR-0039's landed follow-up). That
   hand-rolled panel client is retired; it was the FIRST production consumer of the
   aligned async surface (ADR-0027); R3 asks: lift the pattern into okm as a
   shared wire client.

The shape split is forced, not chosen: `RemoteStore` (sync, ADR-0010) blocks
on `mpsc::recv()` — inside a browser (no runtime to block on; re-entrant
`block_on` panics even natively, see the async module header) a sync WS
sender is structurally impossible. So the browser gets TWO clients of
different traits: sync engine (localStorage) + async wire sender.

## Decision

1. **`LocalStorageStore`, okm-core feature `localstorage`** — implements the
   SYNC `VirtualStorage` against `web_sys::Storage`:
   - `put`/`get`/`del` → `setItem`/`getItem`/`removeItem`; bytes cross as
     base64 — ADR-0018's exception applies exactly here: the host API
     physically holds strings only, base64 is the boundary artifact.
   - `scan_range` → enumerate all keys (`length` + `key(i)`), decode to
     bytes, filter the `[begin, end)` interval, SORT IN RUST — JS string
     order is UTF-16 unit order, NOT UTF-8 byte order; comparing enumerated
     strings directly would misorder keys above U+FFFF. The O(n) enumeration
     scan is accepted at UI-state scale (thousands of rows, ≤ a few MB
     quota).
   - `commit_batch` → the trait default replay (no engine-level atomicity —
     documented boundary: UI state tolerates partial writes; anything that
     must not drift goes through the node that owns it).
2. **`WireClient<T: WireTransport>`, okm-async, implements `VirtualStorageAsync`** (ADR-0059 moved the async face out of core; the trait and the sender ship together)
   — the generic form of the panel's RemoteStore:
   - transport INJECTED (`fn post(bytes) -> Result`), no platform import: the
     client never learns whether it rides a WS message, a UDS datagram, or an
     in-process pump. The receiver-side intake is already transport-free
     (`NestStorage::apply`); this makes the sender side symmetric.
   - the caller pumps inbound responses via `deliver(bytes)`; the client
     answers WAITERS IN FIFO ORDER — legal because the receiver applies a
     connection's frames sequentially (order = send order) and write frames
     never enqueue a waiter (no response by protocol).
   - waiters are a hand-rolled std-only oneshot cell (waker registration),
     no tokio dependency — the crate must stay wasm-buildable under this
     feature alone.
   - transport failure surfaces as explicit `Err` to outstanding waiters
     (no silent park) — the mudra panel's断线 rule, generalized.
3. **`VirtualStorageAsync` moves out of `slatedb_backend.rs` into
   `engine/storage.rs`** — the aligned surface is the engine contract
   (ADR-0027), not a slatedb property. The async trait becomes feature-free
   (AFIT, `#[allow(async_fn_in_trait)]` rationale moves with it).
4. **Typed layer unchanged** — `Collection`/`Graph` still bind sync
   `VirtualStorage`; browser typed data rides `LocalStorageStore`, remote
   access speaks `WireClient` over raw bytes. Type semantics stay node-side
   (the same boundary ADR-0010 drew for remote backends).

## Alternatives considered

- **IndexedDB backend** — rejected: async origin cannot satisfy the sync
  typed layer, and for the async wire client it would only add a second
  shape with no consumer. localStorage was the stated R3 landing.
- **Sync `VirtualStorage` over WS via `block_on`** — rejected as structurally
  impossible (Context); the wasm main thread parks forever.
- **Ship the panel's RemoteStore as-is instead of a generic client** —
  rejected: the FIFO pairing, explicit-Err teardown, and write-frame silence
  are wire-CONTRACT facts discovered by the first consumer; keeping them in
  mudra means the next wasm consumer re-implements (and re-discovers) them.
  R3 already ruled the landing: okm, with its test discipline.
- **A client handshake / version header on the wire** — rejected: frames
  carry no semantics, layout versioning is the sender's in-process concern
  (existing reuse-contract decision; do not re-propose).

## Boundaries

- `LocalStorageStore` is for browser-local state ONLY. Business writes keep
  their single control point: shape-preserving mutations go through the
  owning node's surface (mudra: the 8899 verbs), never a direct client write.
- FIFO pairing is per CONNECTION with a sequential-apply receiver; multiple
  concurrent connections need one `WireClient` each.
- `scan_range_iter` on `WireClient` uses the async trait's buffered default
  for now; a paged `OP_SCAN_STREAM` override is the first performance lever
  when a host needs it (chunk grammar already on the wire, ADR-0021).

## Consumer

mudra R3: the panel's `remote.rs` migrates to `WireClient` (its hand-rolled
pairing becomes the crate's test baseline — ported to okm as the native
in-memory-transport suite); panel local state (`filters`/`collapsed`/
`sortNew` — today in-memory signals) gets `LocalStorageStore` behind the
`localstorage` feature. Gated by the mudra offline criterion:
`cargo check --target wasm32-unknown-unknown` with the feature on.

## Withdrawal (2026-11-10): the sync browser engine

`LocalStorageStore` is gone. The record above says what it was for, and it is kept that way;
what follows is why it was removed rather than the other way round.

**Its only consumer stored two keys of UI state.** The mudra panel persisted `filters` and
`collapsed` — a few dozen bytes, best effort, defaults win when absent. An engine that
implements a full `VirtualStorage` (scan over every key on the origin, batch, remote handle)
is the wrong tool for a cache of two blobs, and the panel now holds `web_sys::Storage`
directly in ~30 lines with the same byte shape.

**Core's dependency closure is a checked boundary.** ADR-0059 removed tokio and slatedb from
it and added a CI check, because a store library that carries an async runtime and an object
store is a store library every consumer pays for. `web-sys` was the same impurity one layer
over, and — unlike the local engines — this backend could not be exercised by a single test
in this repository: its only target is `wasm32`, and the one testable piece was the base64
crossing.

**What the argument does not license.** A browser-local *engine* is still a reasonable thing
to want — an app whose whole dataset lives in the page. That engine cannot be a `VirtualStorage`
(core is sync, and no browser store with durability is), so it belongs in `okm-async`, beside
`SlatedbStore`, as an implementation of `VirtualStorageAsync`. SurrealKV is the obvious
candidate on paper (synchronous point reads and iteration, asynchronous commit pipeline,
Apache-2.0, `wasm32` build in CI) and is what to evaluate when such a consumer appears, with
the constraints its own README states: a `Tree` mounts in a **dedicated Web Worker** over OPFS,
one worker owns a database at a time, and the wasm target is Tier 2 (built, not natively
tested). Its link cost, its Tokio-runtime requirement and the rest of the measurement are in ADR-0064, which defers it. None of that changes this withdrawal: a UI-state cache is not a reason to put an LSM
engine in a panel.

**What stands.** The other half of this ADR is untouched: `WireClient` + `WireTransport` are
the async surface a browser uses for everything that is not local, and the panel's remote path
still goes through them.
