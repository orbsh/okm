# ADR-0026: VirtualStorage writes take &self — the shared-handle contract, stated once

> **Languages:** [English](0026-virtualstorage-shared-handle-writes.md) (primary) · [中文](0026-virtualstorage-shared-handle-writes.zh-CN.md)

**Status:** Accepted (2026-09-26) — design; implementation pending, see Consequences

## Context

`VirtualStorage` declares writes as `put(&mut self)` / `del(&mut self)`
(engine/storage.rs) while reads are `&self`. The `&mut` was never load-bearing:

1. **Every shipped engine already synchronizes internally.**
   - `FjallStore`: `#[derive(Clone)]` with the explicit contract "Clone IS a
     shared handle (Arc-inner)" — fjall's `Database`/`Keyspace` are internally
     synchronized; the impl body calls `&self` methods.
   - `SlatedbSync`: fields are `Arc<Runtime>` + `Db`; `put_sync(&self)` /
     `del_sync(&self)` exist and the trait impl just forwards to them.
   - `RedbStore`: wraps an `Arc<Database>`; `begin_write` takes `&self` (redb
     requires interior mutability by design). First-write table creation works
     the same through `&self`.
   - `TestStore` arms: `Arc<SlatedbSync>` or owned synchronized handles.
   In each impl, the `&mut` receiver is narrowed straight back to `&self` calls.
   It is never used to hold exclusive state across the operation.

2. **okm already contains the counterexample trait.** `VirtualStorageAsync`
   (slatedb_backend.rs) declares the identical op set — including
   `async put(&self)` / `async del(&self)`. The sync trait's `&mut` contradicts
   the async mirror: two statements of one contract, one of them false.

3. **`SharedVirtualStorage` states the real semantics one trait up.**
   `shared_handle()` returns "a handle to the same physical engine. Cheap;
   shares all state." A handle whose clone shares all state and whose writes
   demand `&mut` is a contract that only compiles behind an outer `Arc<Mutex<>>`
   — and that is exactly what every real consumer pays today.

The cost is not hypothetical. aura's `MqStore` wraps its engine in
`Arc<Mutex<MqEngine>>` for no reason but this signature, and its realm/ns-derived
handles re-wrap the inner engine (`Arc::new(Mutex::new(inner.lock().clone()))`),
silently changing the lock boundary per handle. The `&mut` on the trait is the
root: it forces consumers to manufacture `&mut` paths that do not correspond to
any real exclusivity at the data layer.

## Decision

1. **`put` and `del` take `&self`.** `scan_suffix`/`scan_range`/`get` are
   already `&self`; after this change every method of `VirtualStorage` shares
   one receiver, and the trait states one thing: an engine handle is a shared
   handle, writes included. Engine-internal synchronization (fjall, redb, slatedb
   runtime) is where exclusivity actually lives.

2. **`KvBatch` keeps `&mut`.** Batch accumulation is genuinely stateful (an op
   list being built); `batch()`/`commit_batch()` on the engine keep `&mut` as
   well — a batch's accumulation phase is single-owner by construction. The
   relaxation is about the engine handle, not about every `&mut` in the file.

3. **No compat alias, no split trait.** A `VirtualStorageShared` side-by-side or
   a default-method trick preserves two spellings of one contract — the drift
   this ADR removes. One trait, one receiver; all impls move in the same commit.

## Honest semantic cost

- A consumer that relied on `&mut` for cheap single-threaded access reasoning
  (e.g. `Rc`-like engines without interior synchronization) can no longer
  implement the trait without adding a lock or cell. No shipped engine is in
  this class — every one is already synchronized — but a future engine that
  wanted to be `!Sync`-by-construction now wraps its own Mutex internally. The
  exclusivity requirement moves to the engines that need it, instead of being
  imposed on all consumers.
- The relaxation widens what a handle permits (writes from shared refs). It
  does not change any runtime behavior of the engines themselves; their
  internal locking is untouched.
- The downstream sweep includes every consumer `impl` outside okm-core: aura's
  `MqEngine`/`MqStore` and prism's echo-plane adapter. Their `&mut` plumbing
  (`let mut store = mq.clone()` patterns) simplifies mechanically in the same
  wave.

## Consequences

- okm-core: trait signature change in engine/storage.rs; 8 impls updated
  (`FjallStore`, `SlatedbSync`, `RedbStore`, `TestStore`, `RemoteStore`, the
  nest `TestEngine`, the obj_dict test `Engine`, plus any doc-comment
  references). `FjallStore::put`/`del` drop their `&mut` and forward to the same
  `&self` fjall calls; `RedbStore` likewise. Full test suite runs (all engines
  the build carries, per the TestStore matrix).
- aura consumes the relaxed trait in the same wave: `MqStore` drops the
  `Arc<Mutex<>>`, becomes `{ prefix: Vec<u8>, engine: MqEngine }`;
  `for_realm`/`ns_raw` become pure prefix assembly (no re-wrap). This deletes
  the step-1 fix in aura ADR-0030 — the terminal shape lands directly. Per the
  cross-repo rule the sibling (okm) lands first; aura's ADR-0030 records this
  ordering change.
- Consumers that pass `&mut S` to generic helpers (`store_exec`-style seams)
  narrow to `&S`; callers who held owned locals to manufacture `&mut` simplify.
- No PLAN phase in okm; this is an API contract correction, recorded here and
  in the consumer repos' ADRs.
