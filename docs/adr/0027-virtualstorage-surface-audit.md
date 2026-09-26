# ADR-0027: VirtualStorage surface audit — derived conveniences leave the trait

> **Languages:** [English](0027-virtualstorage-surface-audit.md) (primary) · [中文](0027-virtualstorage-surface-audit.zh-CN.md)

**Status:** Accepted (2026-09-26) — implemented in the same pass (tests green:
34 suites, `test-engines,arrow,parquet`).

## Context

The mudra full-stack rewrite (mudra `docs/ADR-rust-fullstack.md`) makes the
panel the first production consumer of `VirtualStorageAsync`, which forced the
question: is the engine trait's surface minimal? Call-site counts below are
grep results over okm + aura + probe (2026-09-26), not estimates.

The sync `VirtualStorage` carried 9 methods; the async mirror 5. Three
suspects:

1. `scan_suffix_kv` — a trait method with a default derived from
   `scan_suffix` + `get` (N+1 lookups). Five call sites, all inside okm's
   model layer. No engine ever overrode it.
2. `batch()` / `commit_batch()` as a pair, with a `KvBatch` trait behind them.
3. `shared_handle()` on its own sub-trait — audit verdict: correct as is, kept.

## Decision

1. **`scan_suffix_kv` leaves the trait** and becomes a free generic function.
   Its body now derives from `scan_range_iter` — ONE pass yielding
   (suffix, value), strictly better than the old N+1 default. Nothing to
   override: `scan_range_iter` is already the engine's native-value pair
   scan.
2. **`batch()` leaves the trait; `commit_batch(ops)` stays.** The commit
   point must hold the engine: fjall executes the op list in its own
   `Batch`, redb in ONE write transaction (both override `commit_batch` —
   the atomicity is real). `batch()` returned the CONCRETE `MemBatch`, so
   no engine could ever wrap a different carrier — its three "overrides"
   (fjall / redb / RemoteStore) were one-line bodies copying the trait
   default. The old fjall doc ("batch() returns MemBatch for encoding;
   commit replays into fjall's own Batch") described the commit honestly
   but sold `batch()` as an extension point it could never be.
3. **The `KvBatch` trait is deleted; `MemBatch` keeps inherent `put`/`del`.**
   Evidence: one implementation, of its only default carrier; `KvBatch::commit`
   had ZERO callers (all `.commit()` hits in the tree are engine internals —
   fjall's `wb.commit()`, redb's `txn.commit()`); `save_into(&mut impl
   KvBatch)` could never receive a non-`MemBatch`. The accumulation API is
   identical after deletion — inherent methods, zero call-site churn.
4. **`VirtualStorageAsync` gains the aligned surface.** Counted against the
   PRE-audit sync trait it lacked 4 methods (`scan_range_iter`,
   `scan_suffix_kv`, `batch`, `commit_batch`); against the post-audit
   surface it mirrors, 3 (`scan_range_iter`, `scan_suffix_kv`,
   `commit_batch`) — landing: async `scan_range_iter` (buffered default =
   the sound mirror of the sync trait's own default; slatedb overrides with
   a ONE-pass native `DbIterator` materialization — NOT the lazy
   `SlatedbIter` wrapper: its `next()` `block_on`s and would panic inside
   the async caller's runtime; the sync world's `SlatedbSync` is the lazy
   adapter's only legal home), async `commit_batch` (replay default;
   SlatedbStore overrides with a native `WriteBatch` + `await_durable` —
   slatedb's real single-write atomic form), suffix-pair scan as a free
   generic function (one trait, both worlds). The async trait was already
   `&self`-native; ADR-0026 §2 named it as the counterexample trait.

Net surface: put / get / del / scan_suffix / scan_range / scan_range_iter /
commit_batch — seven methods, each with ≥13 external call sites except
`scan_range` (13, and load-bearing: the eager single-frame read on the
remote path; deriving it from the iterator would split one OP_SCAN frame
into a paged stream).

## Why Not

- **Keep `KvBatch` as "future extensibility."** The extensibility was
  structurally dead: `batch(&mut self) -> MemBatch` pins the carrier type,
  so the trait could not be satisfied by a native builder. A slot that
  cannot be filled is ceremony, not flexibility (Occam: entities must
  self-justify against a real problem).
- **Make `commit_batch` take `impl KvBatch`.** Same trap one layer up —
  only `MemBatch` implements it, and the engines consume `&[(key,
  Option<value>)]`, which IS `MemBatch.ops`. Passing the list directly
  states the one thing the engines actually need.
- **Delete `scan_range` as "derivable from `scan_range_iter`."** Derivable
  in code, wrong in protocol: `RemoteStore::scan_range` is ONE OP_SCAN
  round trip; the derived form would page (ADR-0021 chunk protocol) for
  callers who want the buffered answer. Keep both; they are two costs, not
  two shapes.

## Future form (deferred, trigger recorded)

If a confirmed need appears for streaming ops into a NATIVE builder without
the intermediate `Vec` (huge batches), or engine-level per-batch options
(compression, ttl), the correct shape is an associated type:

```rust
trait VirtualStorage {
    type Batch: KvBatch;
    fn batch(&self) -> Self::Batch;   // builder born holding the engine handle
    // no commit_batch: commit lives on the builder
}
trait KvBatch {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>);
    fn del(&mut self, key: &[u8]);
    fn commit(self) -> Result<(), String>;
}
```

Model layer composes by generality over the builder
(`save_into<B: KvBatch>(&self, batch: &mut B, ..)`); callers who store a
builder name it `S::Batch`; the common shape (build locally, commit inline)
never spells it.

Honest cost list (corrected 2026-09-26 after the dyn question):
- dyn compatibility: NEARLY ZERO loss. `&dyn VirtualStorage` exists at two
  sites only — `variant_column` (a private parquet helper; generics are a
  one-line change) and the derive-generated EMPTY hook
  `__okm_embed_deref` (body `{}`, unused param). Multi-engine runtime
  dispatch in okm/aura is done with static ENUMS (`TestStore` variants,
  `MqEngine::Fjall|Test`) — the erasure path was never taken.
- Real cost: every enum-forwarding impl grows an enum builder (match arms
  per method); `NestStorage`'s read-flush pattern (empty `commit_batch` to
  make prior frame writes visible to later reads) becomes
  `store.batch().commit()` — for redb that opens a real transaction where
  today's empty op list costs one no-op commit.
- Benefit today: none observed (largest batch ≈ 100 ops, engine-matrix
  test). Defer until the trigger fires.

Update (2026-09-26, after the NoopBatch question): the associated-type form
is ruled out as a universal contract — not "unsupported yet", structurally
impossible for redb. `WriteTransaction<'db>` borrows `&self` and redb 4.2.0
has no owned transaction type; an associated `type Batch` must be owned
(returned by value from `fn batch(&self)`), so redb can only fill it with an
op-list carrier (MemBatch renamed) or a NoopBatch placeholder. A placeholder
degrades either to per-op replay (losing redb's today-real single-
transaction atomicity — regression) or to a Vec inside the "placeholder"
(= MemBatch with a new name). Neither beats the status quo. fjall and
slatedb CAN host native builders (fjall `Batch` is owned; slatedb
`WriteBatch::new()` is detached) — but a unified contract is capped by its
weakest engine, and this is the same failure shape as `KvBatch::commit(self)`:
a builder signature without the engine's hands never reaches the engine's
commit point. If the streaming trigger fires, the correct move is NOT
associated types on `VirtualStorage`; it is per-engine native builder APIs
lifted to the surface where they exist (okm's generic layer serves what it
can), a different trait, not a hole in this one. The current
`commit_batch(op_list)` stands as the minimal common denominator contract:
for redb, commit-time begin_write + fill IS the native form.

## Mitigations

- Call-site audit is reproducible: the counts above came from grep patterns
  recorded in the session; re-run before re-auditing.
- The free function keeps the old name, so model-layer diffs are
  mechanical (`store.scan_suffix_kv(&p)` → `scan_suffix_kv(&store, &p)`).
- Async-side work (mudra R3) inherits the SMALLER surface: 7 sync methods
  to mirror, not 9.
