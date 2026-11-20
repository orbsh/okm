# 0072 — The ephemeral engine is a packed B+tree, because the win is the packing

Status: decided and landed 2026-11-20. `okm-core/src/engine/ephemeral.rs` replaces `engine/mem_store.rs`; the numbers below come from `okm-core/examples/ephemeral_cost.rs` on this machine (40 000 entries per family, a counting allocator, no RSS — `store.bytes()` agrees with it).
Chinese: [0072-ephemeral-engine.zh-CN.md](0072-ephemeral-engine.zh-CN.md)
Depends on: ADR-0020 (a scan is lazy), ADR-0027 (the surface audit), ADR-0045 (capture is a decorator), ADR-0059 (an engine can live outside core)
Related: aura ADR-0050 §6–§7 (a temporary realm writes nothing), `docs/ideas/schema-aware-index.md`, `docs/internals/query-cost-attribution.md`

## Context

The in-memory engine has never been designed as an engine. It was the test map: a `BTreeMap` behind a mutex, `#[cfg(test)]`, about fifty lines — published only because a host wanted a memory mode instead of growing a stand-in of its own.

What makes it worth owning is the premise the project is built on: a model is declared once — keys, indexes, faces, scans, reduces — and the engine is chosen at assembly (README, *shape fixed at declaration, engine chosen at assembly*). **Durability belongs to that choice, not to the model.** A system whose modelling is complex enough to need okm's declarations, and whose lifetime may or may not have to survive the process, should not be forced into two implementations of itself; the same declarations must run over a durable engine or over this one, with only the guarantees changing. aura's temporary realm (`RealmSet::create_ephemeral`, ADR-0050 §6) is the first consumer of that mode — a compute engine that stores its state in okm and runs computations that do not persist — not the definition of it. The semantics change first, and the shape follows.

Three measurements decide the shape.

**The overhead is per entry, not per key byte.** A `BTreeMap<Vec<u8>, Vec<u8>>` pays two `Vec` headers (48 B) whether or not the value is empty, a malloc header per key block, and its node's share — 93–104 B on an entry whose key is 11–19 B. An index entry with no `includes` stores an empty value and still pays all of it. So "save the key's memory" has a ceiling of ~95 B/entry, and compressing the key bytes themselves addresses almost none of it.

**Packing is the lever; the tree shape is a second-order choice.** Anything that stores `K` and `V` as Rust values reproduces the 93–104 B, including every third-party ordered container: their API is `K: Ord, V`, and a variable-length key IS an owned `Vec<u8>` there. The win comes from replacing that with a 12-byte slot pointing into a shared arena of `[key][value]` records addressed by `u32`.

**A prefix tree does not pay for itself here.** A path-compressed radix over the same corpus (exact node census, ART node sizes, `u32` child indices) lands within ±10 B of a packed array on four of six families and loses on high-entropy fixed-width tails. It wins big in exactly one place — a fixed-width key ending on a node, where the path can BE the record — and that single win costs the store knowledge of okm's key layout. Packing already removes the overhead the tree would have removed.

Within the packing, the measured comparison is B+tree against skip list against `BTreeMap`, same corpus, same allocator:

| family | key / value | `BTreeMap<Vec,Vec>` | skip list | packed B+tree |
|---|---|---|---|---|
| primary row (u64 id) | 11 B / 120 B | 224 B/entry | 162 | 144 |
| uuid7 row | 19 B / 120 B | 232 | 171 | 153 |
| index `by_org` | 16 B / empty | 109 | 48 | **31** |
| index `by_org_status` | 17 B / empty | 109 | 49 | 42 |
| covered index | 13 B / 8 B | 114 | 52 | 43 |
| dynamic path | 33 B / 60 B | 185 | 125 | 117 |

A skip list pays a 24-byte header per entry and a pointer chase per step; a B+tree pays one 12-byte slot in a shared node and walks a contiguous array. The B+tree is both smaller and faster, and its only extra cost is rebalancing.

## Decision

1. **One type, one name: okm's non-persistent mode.** `EphemeralStore` (`engine::ephemeral`) replaces `MemStore` — the same declarations over an engine that promises no durability, chosen at the assembly site like any other engine. aura's temporary realm is one caller; any model that must run without persisting is another. `okm-core`'s unit tests are a second caller for a cargo reason (a dev-dependency leading back to this package makes cargo build it twice, and the two copies' `VirtualStorage` traits are distinct types — the double-build problem ADR-0059 records). It also joins `okm-async`'s `TestStore::matrix()`, so every test that iterates the matrix — prefix scans, batch atomicity, `ScanIter::seek`, handle sharing, and the table/graph/reduce workloads — runs against it once per build. Being a store a host relies on is not a licence to be exempt from the contract suite.
2. **The obligations it drops are dropped, not deferred.** No journal, no recovery, no compaction, no generations, no migration, no snapshot across a scan, no cross-process access, no changelog capture. Each is a rule about bytes at rest, and this store has none: the `ChangelogStore` decorator is simply not installed around it, and a crash loses it, which is what a scratch store already promises.
3. **The obligations it gains are the ones a test map never had.** Creation is O(1), `drop` returns every byte (nothing is process-global), every allocation is charged to the store (`len()`, `bytes()`), and the whole ordinary contract holds: byte order, read-your-writes, `shared_handle` = the same keyspace, errors riding scan items.
4. **The shape is a packed B+tree over the full key**: leaves of 32 `{record offset, key length, value length}` slots, inner nodes of 64 `(separator, child)` pairs where the separator is the SMALLEST key of a child's subtree. The min-key convention is what makes okm's append-heavy keys cheap: appending never touches a separator, while the max-key convention would rewrite one per insert.
5. **Splits are biased at the ends.** A monotonic key stream (ids, timestamps, uuid7) splits at the edge — 32/1 rather than 16/17 — because a half-filled leaf that will never be touched again is permanent waste. Measured on the same corpus: 43 B/entry with an even split against 31 B/entry biased. Random-order inserts keep the even split.
6. **No `Vec` in the store doubles.** Records, leaves and inner nodes each live in fixed-size chunks addressed by a `u32` index. A growing `Vec` holds up to twice its live bytes at rest, which on 40 000 index entries is 16 B/entry — most of what the packing was for.
7. **A record is `[key][value]` contiguous in one chunk.** An empty value costs no bytes. A record never spans a chunk; one oversized record owns a chunk of its own (the model layer splits big values into their own table, ADR-0036, so this is a guard).
8. **Delete is the B+tree's own repair**: borrow from an adjacent sibling under the same parent when it has room, otherwise merge and drop the parent's entry, repairing upward and collapsing a root with one child. In-order links cross parents, so a sibling only qualifies when it is the adjacent CHILD of the same parent — merging across a parent boundary would orphan an entry. Record bytes freed by deletes are reclaimed by one **rewrite** — live entries extracted in order, a fresh store built aside, swapped only if complete — triggered when waste exceeds half of live bytes above 64 KiB.
9. **A batch is one write lock.** The trait default takes the lock per op, which on an in-memory engine is not a micro-difference: it is the difference between a concurrent reader seeing a two-op batch whole or in halves.
10. **Reads take a read lock, writes a write lock.** Computations read far more than they write, so a scan does not queue behind another scan, and a point read does not queue behind an iterator.
11. **A scan re-locks per item and keeps an epoch-checked cursor.** While the epoch is unchanged the next item is one slot; after any write the cursor is discarded and the iterator re-descends from the key it last handed out — so a scan never restarts, never repeats, never skips forward. There is **no snapshot across items**: a write between two items may change what comes next, which is the contract the remote backend already states for a paged scan (ADR-0021) and the price of not materializing the range. Every item is copied under the lock, so a key is never paired with another entry's value.
12. **The `u32` reference limit is an error, not a truncation.** A record or a node table that would cross 4 GiB fails the write (`Error::Engine`), because a store that size is a deployment error and rewriting addresses silently would corrupt every slot.

## Measured

40 000 entries per family, one process, heap bytes from a counting allocator:

| family | key / value | `BTreeMap<Vec<u8>,Vec<u8>>` | `EphemeralStore` | ratio |
|---|---|---|---|---|
| primary row (u64 id) | 11 B / 120 B | 224 B/entry | 144 B/entry | 1.55× |
| uuid7 row | 19 B / 120 B | 232 | 153 | 1.52× |
| index `by_org` | 16 B / empty | 109 | 31 | **3.47×** |
| index `by_org_status` | 17 B / empty | 109 | 42 | 2.61× |
| covered index | 13 B / 8 B | 114 | 43 | 2.66× |
| dynamic path | 33 B / 60 B | 185 | 117 | 1.58× |

A document-heavy face is bounded by its payload: 120 B of payload plus ~24 B of structure is 144 B, and no structure improves that. The index faces — the ones a store has most of, one per declared index per row — are where the ratio lives, and there the structure costs 13–25 B/entry instead of 93–104 B.

The cost side, same run: point `put` 124–282 ns against 95–178 ns, point `get` 239–467 ns against 182–311 ns (1.1–1.5×), a 30 000-entry store builds in 4.4 ms against 3.0 ms, and a full scan takes 2.05 ms forward / 3.96 ms backward against 0.95 ms for the old path that materialized the range as `Vec<(Vec<u8>, Vec<u8>)>` — the old path's peak WAS the result set; this one holds 2.75 MiB and nothing more.

## What this does not reopen

- **The disk engine's index layer.** `docs/internals/query-cost-attribution.md` measured that an index-layer rewrite on fjall has a 1.20× end-to-end ceiling, and that stands. This decision adds an engine; it replaces no engine and changes no key format.
- **The schema-aware prefix tree.** Still recorded and not designed (`docs/ideas/schema-aware-index.md`), now with a trigger: an index face whose key bytes are the footprint. The measurement above says the trigger is currently false, because the pack already removes the overhead the tree would have removed.

## Rejected

- **A third-party ordered container.** Checked against the crates as they exist: `scc::TreeIndex` (concurrent B+tree, 15-slot leaves, lock-free reads), `crossbeam-skiplist` 0.1.3, `beets` (in-memory B+tree with a pluggable node allocator and a slab arena), `btree-plus-store` / `btree-slab` (arena-backed `BTreeMap`), and the ART crates (`oxidart`, `art-tree`, `congee`, `subms-adaptive-radix-tree`). Every one is generic over `K`/`V`, so a variable-length key is an owned `Vec<u8>`: a 24 B header plus a malloc block, per key and per value, which is precisely the 64 B/entry this engine exists to remove. They would hand back the 109 B/entry figure with a different tree inside it. Second, their range iterators borrow the container, so they cannot live inside the `'static` `ScanIter`; the only way to use them is to materialize — the memory peak the store exists to avoid.
- **A skip list.** It was the first landing and it works; it costs a 24 B node header per entry plus a pointer chase per step, and measures 48 B/entry against the B+tree's 31 on the same face. Replaced.
- **An adaptive radix tree.** Measured above: ±10 B/entry against a packed array on four families, one real win (fixed-width key ending on a node) that costs the store knowledge of okm's key layout. Recorded with its trigger instead of built.
- **A head→face table (`[ns][slot]` prefix) as a first step.** After packing it is worth 4 B/entry and scan locality, not the 4×: the 64 B it would have saved were the two `Vec` headers and the malloc headers, which the arena already removes. It also imports the key-layout coupling.
- **A buffered scan.** It re-introduces the memory spike the store exists to avoid: a `LIMIT 10` over a large range would materialize the range first (ADR-0020 ruled the opposite).
- **Letting the trait default replay `commit_batch`.** On an in-memory store the ops are already local, so the guard is the entire cost of atomicity, and skipping it buys nothing.
- **A memory ceiling inside the engine.** A cap needs an accounting model and an error the model layer must interpret; `bytes()` exposes the number and the host decides. Revisit if a host grows runaways it cannot see.
- **Journaling the store so an ephemeral store could be resumed.** That is a durable engine with extra steps: pick fjall and the model is unchanged.

## Consequences

- **aura**: `MqEngine::Mem(...)` becomes `MqEngine::Ephemeral(...)` and `MqStore::mem()` builds an `EphemeralStore`; a temporary realm is still not wrapped in `ChangelogStore`. The dependency is a git branch, so downstream picks the rename up with `cargo update -p okm-core` after this lands upstream.
- **`okm-core`'s unit tests and `okm-vector`'s local test map** keep their own shapes; only the published engine moved, and `okm-core`'s tests now run on the real engine.
- **The engine matrix gained an arm.** 733 tests pass with `EphemeralStore` in `TestStore::matrix()`, including the table, graph and reduce workloads that iterate it — which is the only evidence that matters for a backend.
- **ADR-0059's description** of the store ("a `BTreeMap` behind a mutex, `#[cfg(test)]`, about fifty lines") was already false and is corrected there in the same pass.
- **The fill factor is a property of the insert order**: monotonic streams land at ~97 % leaf fill, random ones at ~65 %. That is the price of not scanning on insert, and it is measured rather than assumed.