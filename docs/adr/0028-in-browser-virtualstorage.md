# ADR-0028: In-browser VirtualStorage family — localStorage engine + async wire client

> **Languages:** [English](0028-in-browser-virtualstorage.md) (primary) · [中文](0028-in-browser-virtualstorage.zh-CN.md)

**Status:** Accepted (design; implementation pending user go-ahead, per the
ADR → PLAN → code rhythm).

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
   write frames fire-and-forget. It is the FIRST production consumer of the
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
2. **`WireClient<T: WireTransport>`, okm-core, implements `VirtualStorageAsync`**
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
