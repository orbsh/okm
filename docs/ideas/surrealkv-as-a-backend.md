# Idea: SurrealKV as a backend — measured cost, and the one constraint that decides where it could live

**Status**: candidate, recorded 2026-11-10. Not a decision, not a PLAN item with a date on it, nothing implemented. What follows is what a proposal would have to answer, with the numbers measured on this machine rather than argued from upstream marketing.

**Why it came up**: an outside document (AI-written, 2026-11-10) recommended replacing okm's browser localStorage engine with SurrealKV, and more broadly adopting it as "the" storage engine. The browser half of that recommendation was settled separately — the localStorage backend was withdrawn (ADR-0028 §Withdrawal) because its only consumer stored two keys of UI state, not because SurrealKV is bad. The desktop/server question is this document.

## 1. The short version

SurrealKV can be an okm backend. It **cannot** be a `okm-core` backend: its commit path requires a Tokio runtime, and `okm-core`'s dependency closure is a CI-checked boundary that refuses `tokio` (ADR-0059). So the only honest home is `okm-async`, beside `SlatedbStore`, as an implementation of `VirtualStorageAsync`.

It is also the most expensive of the engines okm already considers: **+2.86 MB of stripped binary against a 0.31 MB baseline**, or 2.3× what fjall costs. That is not a reason to refuse it — it is a reason not to add it before some workload wants what only it has.

## 2. Measured on this machine

Setup: `rustc 1.99.1`, Linux 6.18.60, 16 cores. `[profile.release] lto = "thin"`, `codegen-units = 1`, binaries stripped with `strip -s`. Each bin does the same work — open a directory, insert 1000 fixed-width keys, scan the full range, read one key back — so the difference between two rows is the engine that was linked and actually reached. Baseline is the same bin with no engine.

| bin | engine | stripped | cost over baseline |
|---|---|---|---|
| `bare` | none | 322,256 B (0.31 MB) | — |
| `tokb` | tokio runtime only | 506,184 B (0.48 MB) | +0.18 MB |
| `rdb` | redb 4.5.0 | 1,264,376 B (1.21 MB) | +0.90 MB |
| `fj` | fjall 3.1.13 | 1,605,904 B (1.53 MB) | +1.22 MB |
| `skvt` | surrealkv 1.3.0 + tokio | 3,324,048 B (3.17 MB) | +2.86 MB |

Without LTO (`lto = false`, everything else the same) the ordering and the ratio hold: fjall 1,732,320 B, surrealkv + tokio 3,755,656 B. So **SurrealKV ≈ 2.3× fjall and ≈ 3.2× redb** in link cost, and about 1.64 MB of that difference is surrealkv's own LSM/vLog/encryption code rather than the runtime it needs.

Transitive normal dependencies (inclusive, from `cargo metadata`): **redb 2**, **fjall 74**, **surrealkv 150**, tokio 7 of surrealkv's 150.

For scale: a full okm-based service binary is a few megabytes on its own, so this is a real but not decisive weight — the argument against it is capability, not bytes.

## 3. The constraint: commit needs a Tokio runtime

The same program was built three ways against `surrealkv 1.3.0`:

| how the future is driven | result |
|---|---|
| `futures::executor::block_on(tx.commit())` | panic: `there is no reactor running, must be called from the context of a Tokio 1.x runtime` at `surrealkv-1.3.0/src/task.rs:165` |
| `pollster::block_on(tx.commit())` | same panic |
| `#[tokio::main]`, `tx.commit().await` | works: 1000 writes, full scan, point read all correct |

`surrealkv` spawns onto the ambient runtime; it does not run its commit pipeline inline on the calling thread. Consequences for okm:

- **No `VirtualStorage` implementation.** That trait is synchronous (`put`, `del`, `commit_batch` return `Result<()>` and must have committed), so a wrapper would have to block on a runtime inside `okm-core`. Driving it with `block_on` does not merely degrade it — it panics. Putting a runtime inside `okm-core` also breaks the CI check added in ADR-0059 (`cargo tree -p okm-core -e normal` must not contain `tokio`, `slatedb`, `web-sys` or `wasm-bindgen`).
- **`VirtualStorageAsync` is the fit.** `okm-async`'s trait is plain `async fn` with no `Send` bound on the futures, and `tokio` is already an optional dependency there, pulled only by the `slatedb` feature (ADR-0059: a consumer that wants only the trait is never handed a runtime). A `surrealkv` feature would follow the same shape: `surrealkv = ["dep:surrealkv", "dep:tokio"]`.

## 4. What the integration would have to settle

These are design questions, not chores. They are listed because the shape of the answer decides whether this is a week or a fortnight.

1. **One `Tree` per table, or one `Tree` with namespaces prefixed.** `surrealkv::Tree` is one database at one path. okm keeps one directory per table and, with fjall, one `Database` plus one keyspace per table. Mirroring that with `Tree` means N databases — N manifests, N WALs, N recoveries, and a registry to keep the handles alive. A single `Tree` with `ns ‖ key` prefixes costs one indirection per scan and breaks the "each table's bytes are its own directory" property the changelog model leans on (ADR-0056).
2. **The scan adapter.** `tx.range(start, end)` hands back an `impl LSMIterator` — `seek_first` / `seek_last` / `next` / `prev` / `valid`, with `key().user_key()` and `value()?`. okm's `ScanIter` is a `DoubleEndedIterator`, so the adapter is a handful of lines but real: bidirectional state, tombstones (already filtered inside the engine), and the `LSMIterator` trait has to be in scope. fjall's `range()` yields `Guard`s and needs no adapter — this one is more surface than fjall, less than slateb.
3. **Durability mapping.** `tx.set_durability(Durability::Immediate | Eventual)` lines up exactly with okm's contract that a returned commit is durable, which is cleaner than what `SlatedbStore` has to do. Choose `Immediate` for the trait's default; expose nothing else until a caller measures a need.
4. **Who owns close.** `tree.close().await` is a real step (flush + release the mount), so the store type needs a shutdown path that the async surface currently has no place for — `SlatedbStore` has the same gap; adopting this engine would be a good moment to decide it once instead of twice.
5. **Single mount.** One open `Tree` per path, and the engine refuses a second mount. That is compatible with okm's single-writer rule (ADR-0056), but it makes the multi-process case explicit: two okm instances over one path would fail at `build()`, not at the writer check.

## 5. What it would buy, and what okm does not need

Real capabilities:

- Synchronous point reads and iteration over an LSM with an async commit pipeline — reads never block on the writer, which is what makes it pleasant under the async trait.
- Engine-level MVCC: readers get a consistent snapshot while a commit lands.
- Encryption at rest inside the engine (`SoftwareKeyManager`, XChaCha20-Poly1305, key rotation), plus a CLI (`surrealkv-cli`) that can inspect, scan and scrub a database offline.
- One implementation that also runs in the browser (dedicated Web Worker over OPFS, ADR-0028 §Withdrawal has the constraints).
- Apache-2.0, published benchmarks (vendor numbers, bare metal, 500k keys: ~6.8–7.0M point reads/s, 229k–322k writes/s) that put it well ahead of SlateDB and roughly level with RocksDB on reads.

What okm already answers without it:

- **Version history and consistent reads**: okm's version semantics live in the changelog segments (ADR-0056), not in the engine. A snapshot API is not on any okm surface, and no consumer has asked for one.
- **Encryption**: nothing in okm's model wants the engine to hold keys; if a deployment needs encryption at rest it is a volume job.
- **Coverage**: embedded single-node is fjall (write-heavy) and redb (read-deterministic COW); object-storage async is slateb. A fourth engine duplicates the embedded slot — and §9 measures redb ahead of surrealkv on scans, short range scans and post-restart reads, so the slot it would fill is filled better.
- **Throughput**: none of these numbers are the bottleneck today. The unit of okm work is a document encode plus a segment append; an engine 2× faster at 1000-key inserts moves nothing a user can see (and the last time a store-level number was taken seriously here it was because a specific workload was measured — see the rustfs size note, where measuring beat guessing in both directions).

## 6. What would reopen it

Any one of these, with the measurement in hand:

1. **Multi-process or multi-instance readers that need a snapshot while another process writes.** That is the one capability nothing else in the stack provides, and it cannot be produced from changelog watermarks alone.
2. **Encryption at rest that has to live inside the engine** (a platform that will not encrypt volumes, but will let the application hold a key).
3. **fjall's write path measured as the ceiling** in a real workload — with numbers from this repo's own benches, not a paper.
4. **A single engine for both native and browser.** If a browser dataset ever justifies a real engine (ADR-0028 §Withdrawal describes when that stops being absurd), having the same engine on both sides is worth more than either side's benchmark.

Until then: deferred, and no feature flag. A flag that exists is a flag that has to be built, tested and kept building.

## 7. What NOT to do, whatever is decided

- Do not add a synchronous facade over `SurrealKV` with `block_on`. It does not work (see §3) and, if it ever started working, it would hide a runtime inside `okm-core`.
- Do not implement it to make the browser story "consistent" with the server. The browser constraint is worker-plus-OPFS, which is a different program shape, and the panel it was proposed for holds two keys.
- Do not judge it by crate popularity or by the vendor's benchmark table. §2 exists because the numbers that matter for a library are the ones its consumers pay.

## 8. Reproducing §2 and §3

The measurement project is a five-bin scratch crate; it is not in the repo, and it should not be — it is a scale, not a fixture:

```toml
[dependencies]
redb = "4.2"
fjall = "3.1.13"
surrealkv = "1.3.0"
futures = "0.3"        # block_on, to show it does not work
pollster = "0.4"       # same, second opinion
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }

[profile.release]
lto = "thin"
codegen-units = 1
```

Bins: `bare` (prints `args().count()`), `tokb` (an empty `#[tokio::main]`), `rdb` (redb insert + range), `fj` (fjall `Database::create_or_recover` + `keyspace` + `range::<&[u8], _>(..)`), `skvt` (surrealkv `TreeBuilder` + `begin` + `set` + `commit().await` + `range` + `seek_first`/`valid`/`next`). Build with `cargo build --release`, then `strip -s` each and compare `stat -c%s`. Swap `skvt`'s `#[tokio::main]` for `futures::executor::block_on` to reproduce the panic in §3.

## 9. Performance and memory (measured 2026-11-10)

The comparison that matters is not surrealkv against fjall alone: okm already ships **two** embedded engines, and a read-heavy namespace is served by **redb** (COW B-tree, read-deterministic) today. So all three were measured with the same harness — 500,000 entries of 16-byte keys and 100-byte values, no compression, every commit fsync'd every 1,000 writes, fjall's memtable forced out to SSTs before its settled state is judged, medians of three runs, one process per engine.

| metric | fjall 3.1.13 | redb 4.5.0 | surrealkv 1.3.0 |
|---|---|---|---|
| open a fresh store | 9.1 ms | 0.6 ms | 2.9 ms |
| load 500k, fsync every 1k | 703 ms | **436 ms** | 1099 ms |
| load 500k, fsync every 10k | 486 ms | — | 526 ms |
| full scan of 500k | 103 ms | **18 ms** | 44 ms |
| 200k random point reads (hot) | 475k ops/s | 1.24M ops/s | **1.51M ops/s** |
| 10k short range scans × 100 rows | 198 ms | **41 ms** | 113 ms |
| reopen a settled store | 1.7 ms | 0.3 ms | 1.4 ms |
| 200k point reads after reopen | 35k ops/s | **1.05M ops/s** | 174k ops/s |
| disk after settle | 58.1 MB | 128.5 MB | 59.1 MB |
| peak RSS | 115 MB | 116 MB | 117 MB |
| RSS after reopen | 116 MB | 45–78 MB | 82 MB |
| link cost (§2) | +1.22 MB | **+0.90 MB** | +2.86 MB |
| runtime needed | none | none | **Tokio** (§3) |

The table has one clear loser and one clear winner, and the winner is not surrealkv.

- **redb is the read engine, by a margin that ends the argument.** Full scan 5.7× faster than fjall and 2.4× faster than surrealkv; short range scans 4.8× / 2.8×; and after a restart it reads at **1.05M ops/s against surrealkv's 174k** — a 6× gap, because a COW B-tree needs no recovery and its pages arrive through mmap already cached, while both LSM engines pay a search through levels with a cold block cache.
- **surrealkv's only lead is hot point reads**, 1.51M vs redb's 1.24M — 1.2×, bought with +2 MB of binary, 60 more crates and a Tokio runtime.
- **redb is also the fastest writer** (436 ms against fjall's 703 and surrealkv's 1099). The fjall-vs-surrealkv write gap is fsync plumbing, not throughput: at one commit per 10,000 writes they are even (486 vs 526 ms). okm writes changelog segments and commits per segment, so its effective batch is large.
- **Memory is a wash at peak** (all three are dominated by the write buffer, ~115 MB). Settled RSS differs but does not mean what it looks like: redb's 45–78 MB is mmap'd file pages counted only when touched (and reclaimable); surrealkv's 82 MB is a resident in-memory index; fjall's 116 MB is memtable plus block cache. In a cgroup-limited container redb's real cost is its working set, not its RSS.
- **The price redb pays is disk**: 128.5 MB against 58–59 MB — COW rewrites whole pages, and neither engine was configured to compress.

So the "read-heavy namespaces" use case that argued for this engine is already covered, and covered better and cheaper. What remains of surrealkv is §5's capability list, not its speed: engine-level MVCC snapshots, in-engine encryption, and one engine on native and in the browser.

Caveats: one machine (Zen 5), single process, no compression, 100-byte values, one read style per phase, and per-operation transaction begin for redb and surrealkv (the shape a query layer actually uses). surrealkv's numbers move with commit granularity — at 10k per commit its hot-read phase fell to 193k ops/s and RSS rose to 140 MB on this same data. fjall's numbers are after a forced `rotate_memtable_and_wait`; without it the journal holds everything (122 MB on disk, 410 ms to reopen) and the comparison would be unfair to it.

Reproduce: `/home/master/.hermes/cache/scratch/kvcmp` (`BATCH=<n> ./target/release/{fj,rdb,skv} <dir>`).
