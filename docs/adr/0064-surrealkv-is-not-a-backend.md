# ADR-0064: SurrealKV is not a backend, for now

Date: 2026-11-12
Status: DEFERRED 2026-11-12

**Depends on:** ADR-0056 (the segment is the migration path — where `surrealkv` is qualified as a candidate engine), ADR-0059 (the async face is a crate — the boundary any non-core engine has to live behind), ADR-0028 (the in-browser `VirtualStorage` family, whose §Withdrawal settled the browser half)

## 1. Context

SurrealKV is SurrealDB's in-house embedded key/value store: an LSM tree — memtable, WAL, SSTables, compaction — over a versioned index, so every write lands under a version and a reader keeps a consistent snapshot while a commit is in flight. Its surface is deliberately thin: one `Tree` per path, `begin` / `set` / `get` / `range`, a commit that is async while point reads and iteration are synchronous. It also runs in the browser (dedicated Web Worker over OPFS), carries encryption at rest inside the engine, and is Apache-2.0.

It came up twice in one day, from an outside proposal (2026-11-10): replace okm's browser localStorage engine with it, and — the larger claim — adopt it as "the" storage engine. The browser half is closed and closed for its own reason: the localStorage backend was withdrawn because its only consumer stored two keys of UI state, not because SurrealKV is bad (ADR-0028 §Withdrawal). The desktop/server question is this ADR.

What made it attractive is exactly what the three engines okm already carries split between them. fjall is the write-heavy embedded LSM; redb is the read-deterministic one (COW B-tree over mmap, no recovery); slatedb is the object-storage one, async, behind `okm-async`. SurrealKV is a fourth LSM whose distinguishing properties are engine-level MVCC snapshots, an async commit pipeline under synchronous reads, in-engine encryption, and one engine that also runs in the browser — none of which any of the three provides.

## 2. Measured on this machine

Link cost, `rustc 1.99.1`, `lto = "thin"`, `codegen-units = 1`, binaries stripped. Every bin does the same work (open a directory, insert 1000 fixed-width keys, scan the full range, read one key back), so a row minus the baseline is the engine that was linked and reached.

| bin | engine | stripped | cost over baseline | normal deps (inclusive) |
|---|---|---|---|---|
| `bare` | none | 322,256 B (0.31 MB) | — | — |
| `tokb` | tokio runtime only | 506,184 B (0.48 MB) | +0.18 MB | — |
| `rdb` | redb 4.5.0 | 1,264,376 B (1.21 MB) | +0.90 MB | 2 |
| `fj` | fjall 3.1.13 | 1,605,904 B (1.53 MB) | +1.22 MB | 74 |
| `skvt` | surrealkv 1.3.0 + tokio | 3,324,048 B (3.17 MB) | +2.86 MB | 150 |

So it is 2.3× fjall and 3.2× redb in link cost, and about 1.64 MB of that gap is surrealkv's own LSM / vLog / encryption code rather than the runtime it needs. For a service binary this is weight, not a veto.

The constraint that decides placement: `surrealkv` spawns its commit pipeline onto the ambient runtime instead of running it inline. Driven by `futures::executor::block_on` or `pollster::block_on`, `tx.commit()` panics with `there is no reactor running, must be called from the context of a Tokio 1.x runtime` (`surrealkv-1.3.0/src/task.rs:165`); under `#[tokio::main]` the same program writes 1000 keys, scans and reads back correctly. Therefore no synchronous `VirtualStorage` implementation (that trait must have committed when it returns), and no presence in `okm-core`, whose dependency closure refuses `tokio` by CI check (ADR-0059). The only honest home is `okm-async`, beside `SlatedbStore`, as a `VirtualStorageAsync` implementation — the shape ADR-0059 already prescribed for an engine that lives outside core.

Throughput and memory, same harness on all three engines: 500,000 entries of 16-byte keys and 100-byte values, no compression, every commit fsync'd, fjall's memtable forced out to SSTs before its settled state is judged, medians of three runs, one process per engine.

| metric | fjall 3.1.13 | redb 4.5.0 | surrealkv 1.3.0 |
|---|---|---|---|
| load 500k, fsync every 1k | 703 ms | **436 ms** | 1099 ms |
| full scan of 500k | 103 ms | **18 ms** | 44 ms |
| 10k short range scans × 100 rows | 198 ms | **41 ms** | 113 ms |
| 200k random point reads (hot) | 475k ops/s | 1.24M ops/s | **1.51M ops/s** |
| reopen a settled store | 1.7 ms | 0.3 ms | 1.4 ms |
| 200k point reads after reopen | 35k ops/s | **1.05M ops/s** | 174k ops/s |
| disk after settle | 58.1 MB | 128.5 MB | 59.1 MB |
| peak RSS | 115 MB | 116 MB | 117 MB |
| link cost (§ above) | +1.22 MB | **+0.90 MB** | +2.86 MB |
| runtime needed | none | none | **Tokio** |

SurrealKV's one lead is hot point reads, 1.2× over redb, bought with 2 MB of binary, 60 more crates and a runtime. The read-heavy slot that argued for it is already held, and held better and cheaper: redb is 2.4× faster on a full scan, 2.8× on short range scans, and 6× on point reads after a restart (a COW B-tree needs no recovery and arrives through mmap already cached, while an LSM pays a search through levels against a cold block cache). redb is also the fastest writer here; the fjall-versus-surrealkv write gap is fsync plumbing, not throughput — at one commit per 10,000 writes they are even (486 vs 526 ms), and okm commits per segment, which is a large batch.

Caveats, because they bound the table: one machine (Zen 5), single process, no compression, 100-byte values, one read style per phase, per-operation transaction begin for redb and surrealkv. SurrealKV's numbers move with commit granularity — at 10k per commit its hot-read phase fell to 193k ops/s and RSS rose to 140 MB on the same data.

## 3. Decision

SurrealKV is not integrated at this time. No dependency, no feature flag, no adapter: a flag that exists is a flag that has to be built, tested and kept building. It remains what ADR-0056 recorded — an engine that qualifies on capability — and nothing is built against that qualification.

## 4. Why

**Version history is not the engine's job here.** okm's version semantics live in the changelog segments (ADR-0056); the store is asked for the current bytes and nothing else. A snapshot API is not on any okm surface and no consumer has asked for one, so engine-level MVCC would be capacity nobody spends.

**Encryption belongs to the volume.** Nothing in okm's model wants the engine to hold keys; a deployment that needs encryption at rest encrypts the volume. This is the one capability that cannot be assembled from what is already there, which is why it is also the shortest path back into this decision (below).

**The coverage is already complete.** Embedded write-heavy is fjall, embedded read-heavy is redb, object-storage async is slatedb. A fourth embedded engine duplicates a slot that is filled — and §2 measures the duplicate losing on the reads that would justify it.

**Throughput is not the bottleneck.** The unit of okm work is a document encode plus a segment append; an engine twice as fast at a thousand-key insert moves nothing a user can see. Store-level numbers have been worth acting on here exactly when a specific workload was measured, never when they were a benchmark table.

## 5. Consequences

The backend matrix stays fjall / redb / slatedb, and the async surface stays `SlatedbStore` beside `WireClient`. No crate, feature or CI step is added.

Three things stay closed whatever happens next:

- **No synchronous facade over `SurrealKV` driven by `block_on`.** It does not work today (§2), and if it ever started working it would hide a runtime inside `okm-core`, which is the boundary ADR-0059 exists to keep.
- **It is not added to make the browser story "consistent" with the server.** The browser constraint is worker-plus-OPFS, a different program shape, and the panel that prompted this holds two keys.
- **It is not judged by crate popularity or the vendor's table.** The link cost and the transitives are things a consumer of this library pays; the vendor numbers describe bare-metal point reads on a different workload.

## 6. What would reopen it

Any one of these, with a measurement in hand:

1. **A multi-process or multi-instance reader that needs a snapshot while another process writes.** That is the one capability nothing else in the stack provides and changelog watermarks cannot synthesize.
2. **Encryption at rest that has to live inside the engine** — a platform that will not encrypt volumes but will let the application hold a key.
3. **fjall's write path measured as the ceiling** in a real workload, with numbers from this repository's own benches.
4. **One engine for native and browser.** If a browser dataset ever justifies a real engine (ADR-0028 §Withdrawal says when that stops being absurd), having the same engine on both sides is worth more than either side's benchmark.

The first is a shape question for ADR-0056 before it is an engine question: a snapshot API is a new surface, and it would be asked of the store contract either way.

## 7. This is a deferral, not a rejection of the design

Nothing here says the engine is wrong. Its design is the reason it is the one candidate recorded in ADR-0056, its commit/read split is what makes it pleasant behind `VirtualStorageAsync`, and its browser story is the only real answer to that question among the four. The record argues about cost and about filled slots: 2.3× fjall's link cost and 60 more crates are a reason not to add it before a workload wants what only it has, which is not the same as a reason to refuse it.

So the decision is dated and revisitable on the four triggers above, and the measurements stay in this ADR so the next round starts from them rather than from the vendor's table.