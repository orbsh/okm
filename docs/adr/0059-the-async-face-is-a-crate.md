# ADR-0059: The async face is a crate, so not depending on it means it is not there

**Status**: Accepted and landed (2026-11-10) — `okm-async` exists, core carries no `slatedb`, no `test-engines` and no `tokio`, and `ScanIter::External` is what the sync adapter hands back. Workspace green: 646 tests, clippy clean.
**Date**: 2026-11-10
**Chinese**: [0059-the-async-face-is-a-crate.zh-CN.md](0059-the-async-face-is-a-crate.zh-CN.md)
**Depends on**: ADR-0020 (a scan is lazy), ADR-0027 (the surface audit), ADR-0028 (the wire sender is a client, not a storage)
**Related**: ADR-0010 (the remote backend carries bytes)

## Context

Core's storage contract is synchronous and has been all along: `VirtualStorage` (`okm-core/src/engine/storage.rs:28`) is `put` / `get` / `del` / `scan_suffix` / `scan_range` / `scan_range_iter`, no `async fn` anywhere. The asynchronous surface is a separate, small thing: `VirtualStorageAsync` (`:377`), a six-method mirror with **exactly two implementors** — `SlatedbStore` (an engine whose API is async) and `WireClient<T>` (a sender over a transport that answers in futures). `SlatedbSync` (`slatedb_backend.rs:596`) drives the async engine back down to the sync contract: it holds one `mpsc::Sender<Cmd>` (`:152`) and a thread runs `drive(rt, db, rx, durable)` (`:164`) with a tokio runtime inside it, one reply per call.

So the async face exists for two reasons and costs two different things, only one of which is visible in the source:

- **Runtime.** `tokio` enters the tree through exactly one feature: `slatedb = ["dep:slatedb", "dep:tokio"]` (`okm-core/Cargo.toml:29`). `WireClient` needs no runtime at all — `wire_client.rs` is hand-written `Future` / `Waker` / `Poll` over `std::future`, no tokio.
- **Boundary.** Everything else lives in core behind feature flags, and a feature flag is not a boundary: cargo unifies features across a build graph, so any crate that enables `okm-core/slatedb` puts `tokio` into every user of `okm-core` in that graph. Core's own `test-engines = ["slatedb"]` (`:28`) does exactly this, which is why every `cargo test --workspace` carries tokio and slatedb whether or not the caller wants them.

Census at the time of the decision (2026-11-10):

- `WireClient` has no consumer outside `okm-core/tests/wire_client_test.rs`. The live remote shape is mudra's `NestStorage<FjallStore>` receiving frames over WS, and aura applying `okm_wire::OpFrame`s to a sync store.
- `TestStore` (`engine/test_engine.rs`) is the matrix over the real backends — slatedb in-memory (zero fs, the default), fjall temp-dir, redb temp-file — and its slatedb arm is `SlatedbSync`. It is used by `okm-core` (48 call sites), `okm-lake` (13), `okm-dynamic` (8), `okm-query` (1) and both bindings (3).
- No crate in `~/world` names `SlatedbStore`, `SlatedbSync` or `WireClient`.

## Decision

**The async face ships as `okm-async`, and core stops carrying it.** The crate holds:

    VirtualStorageAsync        the mirror, with its default methods and its async_fn_in_trait allow
    WireClient / WireTransport the async sender over an injected byte transport
    SlatedbStore / SlatedbSync / SlatedbIter, and the DB-opening helpers
    TestStore                  the engine matrix — it is the enum over the real backends, one of
                               which is slatedb, so it cannot stay where that arm cannot exist

Dependencies point one way: `okm-async → okm-core` (for `Batch`, `ScanIter`, `Error`) plus `okm-wire`; nothing in core names it. Core keeps the whole sync contract — `VirtualStorage`, `Batch`, `ScanIter`, `SharedVirtualStorage`, `NestStorage` (the receiver) and `RemoteStore` (the sync sender over an mpsc transport, `nest.rs:122`), none of which is about async.

**Core gains one variant: `ScanIter::External(Box<dyn DoubleEndedIterator<Item = Result<(Vec<u8>, Vec<u8>)>> + Send>)`.** `ScanIter` (`storage.rs:174`) is a closed enum whose arms are feature-gated (`Fjall`, `Redb`, `Slatedb`, `Remote`, `Buffered`) and whose `Iterator` / `DoubleEndedIterator` implementations live in core (`:192`, `:219`). The `Slatedb` arm leaves with the crate, and the alternative — making the sync adapter buffer the whole range — deletes a guarantee, not an optimization: ADR-0020 makes a scan lazy so that abandoning it half way costs only the reads performed. A type-erased arm is engine-agnostic: it is the door any engine living outside core hands its iterator through, so the next crate that leaves does not need another core change.

**`test-engines` stops being a feature and becomes a dev-dependency** on `okm-async` in the crates whose tests want the matrix. Cargo allows that cycle (`okm-core` dev-depends on `okm-async`, which depends on `okm-core`). `okm-core`'s own dev-dependencies keep `tokio` because its tests use it directly; that is a test-only edge, not part of the contract.

**The boundary is enforced by the build, not by memory.** CI asserts that `cargo tree -p okm-core` (normal edges) contains neither `tokio` nor `slatedb`, and that `slatedb` appears nowhere under `okm-core/src`.

## Why the split axis is async, not the backend

fjall and redb stay where they are, behind features, and that asymmetry is correct rather than an oversight. A crate per backend draws a boundary where nothing varies: `FjallStore` and `RedbStore` are one synchronous byte KV, interchangeable from the contract's side, differing in constants (memory vs file, iteration shape) and in one dependency edge. `slatedb` differs on an axis the type system can see: it implements a *second trait*, needs a runtime, and needs an adapter thread before the sync contract can drive it.

The unit of the split is the unit that varies. Here that is the contract — one synchronous byte KV with a lazy ordered scan — so the contract stays in `okm-core` and everything that speaks the other one goes to `okm-async`. The crate is also named for the axis rather than for a product: the next async engine joins it, and a future async sink or watcher has an obvious address.

The crate is the unit of **removal**, not the unit of the contract. slatedb is a supported engine today, and "we may not want it later" is answered by a directory: delete `okm-async`, drop the dev-dependency lines, and the optional `ScanIter::External` arm (engine-agnostic, harmless when unused). Nothing in core has to be untangled, because nothing in core was ever interleaved with it. That is the property this decision buys, and it is why the question "do we keep slatedb?" does not need answering now.

## Rejected

- **`okm-slatedb` — one crate per engine that leaves.** Same structure, wrong axis: a crate named after a product makes the *next* async backend choose between joining a crate named after a competitor and forking a third one. The asymmetry against fjall/redb is real; naming it after one engine hides it instead of stating it.
- **Keep everything in core behind features (the status quo).** It fails exactly where removal is felt: the variant, the adapter, the feature and the tests interleave, taking slatedb out means surgery on `storage.rs`, `test_engine.rs`, `lib.rs`, `engine.rs` and the test matrix, and feature unification puts tokio into builds that never asked for it.
- **Leave `VirtualStorageAsync` in core and move only the engines and the client.** Cheaper by one file and worse: the two halves of one contract then live in two crates and mirror each other by convention, with nothing mechanical to check they still agree. Today they are adjacent in one file and the audit is a glance. The trait moves with its implementors.
- **Let the sync adapter buffer and skip `ScanIter::External`.** It removes the only change core has to take, at the price of the laziness ADR-0020 ruled: a `LIMIT 10` over a million-row range would materialize the range first. A dynamic-dispatch arm is the cheaper side of that trade.

## What this gives up

- **Adjacency.** The mirror and the trait it mirrors stop being in one file; ADR-0027's "the two surfaces stay aligned" becomes a review rule instead of something the compiler and the reader see at once.
- **One flag moves down instead of disappearing.** `okm-async` needs an internal `slatedb` feature (default off): the mirror and `WireClient` bring no runtime, `SlatedbStore` brings tokio. Anyone wanting only the trait must not be handed the engine.
- **A vtable per item** for scans of engines outside core (`ScanIter::External`). Constant-factor, on a path already crossing a channel per call in the adapter.

## Consequences

Files that move:

    okm-core/src/engine/slatedb_backend.rs   → okm-async/src/slatedb_backend.rs
    okm-core/src/engine/wire_client.rs       → okm-async/src/wire_client.rs
    okm-core/src/engine/storage.rs           VirtualStorageAsync leaves; ScanIter gains External
    okm-core/src/engine/test_engine.rs       → okm-async/src/test_engine.rs
    okm-core/tests/slatedb_eval.rs           → okm-async/tests/
    okm-core/tests/wire_client_test.rs       → okm-async/tests/

Core edits: `Cargo.toml` (drop the `slatedb` and `test-engines` features, the `slatedb` and optional `tokio` dependencies, the `required-features` entries that name them; add the dev-dependency), `engine.rs` (module declarations), `lib.rs` (the `SlatedbStore`, `slatedb_backend`, `WireClient`, `WireTransport` and `test_engine` re-exports), `storage.rs` (`External` plus two match arms), and the `ScanIter::Slatedb` uses removed.

Test fanout: `okm-core`, `okm-dynamic`, `okm-lake`, `okm-query`, `bindings/okm-entry`, `bindings/okm-python` each take a dev-dependency on `okm-async` and change `okm_core::TestStore` to `okm_async::TestStore` at their import sites — mechanical, ~70 call sites, no behaviour change.

Nothing downstream moves: no crate outside okm uses any of the types this crate takes.

One thing the paper could not see, and the landing cost: **`okm-core`'s own unit tests cannot use `okm_async::TestStore`.** A dev-dependency that leads back to this package makes cargo build `okm-core` twice — once for the tests, once for the dependency edge — and the two copies' `VirtualStorage` traits are distinct types, so the matrix can never satisfy the bound. Integration tests do not have the problem (there the lib is a dependency, not the unit under test) and keep using the real engines. Core's unit tests therefore run on `engine::mem_store::MemStore`: a `BTreeMap` behind a mutex, `#[cfg(test)]`, about fifty lines. What it does not cover is engine behaviour — the async adapter's thread and channels, fjall's snapshot semantics — which is exactly what lives in `okm-async`'s tests and the integration matrix.

Reopen on evidence, not on taste:

- slatedb stops being a supported engine → `okm-async` holds only the mirror and an unclaimed sender, and the honest move is deleting the crate rather than keeping a placeholder;
- `WireClient` gains a real consumer outside okm → the crate has a reason independent of any engine;
- a second async engine arrives → the crate is doing the job its name describes.

## History

1. 2026-11-10 — opened as "take SlateDB out of the transaction engine so the storage contract returns to sync". The premise did not survive a read: `VirtualStorage` has no `async fn`, and the only runtime entry is the `slatedb` feature. The question became where the asynchronous surface lives rather than whether the contract is synchronous.
2. Named `okm-slatedb` first, then `okm-async`: the split axis is the contract, not a product.
3. `TestStore` was going to stay in core until the slatedb arm forced the question; it moves with the crate, which is also why the dev-dependency fanout exists.