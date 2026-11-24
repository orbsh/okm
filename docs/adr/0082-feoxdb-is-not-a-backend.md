> **Languages:** [English](0082-feoxdb-is-not-a-backend.md) (primary) · [中文](0082-feoxdb-is-not-a-backend.zh-CN.md)

# ADR-0082 — FeOxDB is not a backend, persistent or ephemeral

**Status:** Decided 2026-11-24 (no code). Closes FeOxDB 0.6.0 as a candidate for both roles it was proposed for.

## 1. What was proposed

FeOxDB (`mehrantsi/feoxdb`, Apache-2.0, crates.io `feoxdb` 0.6.0, released 2026-07-31) is a pure-Rust embedded KV store that advertises nanosecond-scale reads and two modes: memory-only, and persistent behind a write-behind buffer. Inside it is an `scc::HashMap` (23 hash bits, 8 M buckets at the default), a `crossbeam_skiplist::SkipMap` as the ordered index, and one `Arc` record per entry carrying a value, a TTL and a version timestamp. Its write surface is single-key: `insert`, `delete`, `insert_if_absent`, `compare_and_swap`, `atomic_increment`, TTL and JSON-patch variants, plus `flush`.

The proposal was to take it as an optional engine, and — since it also runs memory-only — as a candidate for the non-persistent mode that ADR-0072 gave `EphemeralStore`. Both are refused, and neither refusal is about speed: it links 484 KB lighter than redb.

## 2. What was measured

Link cost, `rustc 1.100.0`, `lto = "thin"`, `codegen-units = 1`, stripped, one binary each, release:

| binary | stripped | over bare | normal deps (unique crates) |
|---|---|---|---|
| `bare` | 320,680 B | — | — |
| + `feoxdb` 0.6.0 (no default features) | 766,080 B | +445 KB | 52 |
| + `redb` 4.2 | 1,250,144 B | +929 KB | 1 |

The dependency closure is the cost that shows: 52 crates, including `io-uring`, `json-patch`, `serde_json`, `intrusive-collections`, `scc`, `ahash`, `crossbeam-*`, `nix`/`libc`, `rand`. ADR-0064 refused SurrealKV for 150 crates spent on capabilities okm has no consumer for; here 52 buy TTL, CAS, atomic counters and RFC 6902 patches, none of which okm calls.

The memory-only mode, `FeoxStore::new(None)`, 40 000 entries, release, system allocator (not its default jemalloc), RSS read from `/proc/self/statm`:

| corpus | RSS/entry | self-reported `memory_usage()` | 40 000 inserts |
|---|---|---|---|
| index face — 19 B key / 1 B value | 3,916 B | 188 B | 47–65 ms |
| row — 11 B key / 120 B value | 4,007 B | 299 B | 50–64 ms |
| row — 11 B key / 2 KB value | 5,954 B | 2,227 B | 67 ms |

The fixed part is what the value-size row exposes: growing the value by 1,928 B moved RSS by 1,947 B, so a record costs ~3.9 KB of process memory before its payload — a block per record, while the store's own accounting counts the record body and neither the index nor the block. Point read of 1 000 keys 470–564 µs, a 1 000-key `range_query` 96–128 µs, `new()` 24–43 µs (+350 KB RSS), and 20 rounds of create → 1 000 inserts → drop settle at a 19.7 MB RSS floor without growing per round.

Against ADR-0072's counting-allocator figures for the same families — its index face is 16 B/empty, the row corpus 11 B/120 B — `EphemeralStore` is 25 B/entry on an index face and 137 B/entry on a row, **157× and 29× denser** than FeOxDB's memory mode (the probe's index key is 19 B with a 1 B value, because the engine refuses the empty one), with point writes of 86–211 ns against its 1.2–1.6 µs (10×) and a 30 000-entry build in 3.5 ms against its ~10× slower per-entry rate.

## 3. Three constraints that are not adapter work

1. **Empty values are refused.** `operations.rs:264` and `:602` (v0.6.0): `if value.is_empty() || value.len() > MAX_VALUE_SIZE { return Err(InvalidValueSize) }`. A probe inserting `b""` fails with `InvalidValueSize`. An index entry with no `includes` stores an empty value, and ADR-0072's own table lists that face as "16 B / empty" — it is the entry type a store holds most of. Writing a placeholder byte and stripping it on read is the encoding changing itself to fit an engine, which is the direction ADR-0011 and ADR-0018 exist to prevent.
2. **`Ok` does not mean the bytes are on disk.** ADR-0039 §1: `Ok(())` from `put` means the store has the bytes, not that the call was accepted. FeOxDB buffers writes — flush every 100 ms or per shard at 1 024 entries / 16 MB, worst case `16 MB × shards` at risk (64 MB at four shards, 128 MB at eight) — and says so itself (`lib.rs:47-49`). Satisfying the contract means calling `flush()` per write, which deletes the reason to use it; not satisfying it breaks ADR-0045's "commit first, append after, at most once" into ghost rows: a log entry for a row the store forgot.
3. **There is no multi-key atomic commit.** The public surface has no batch, no transaction, no commit — every write is one key. okm's write path is primary row + N index entries + reduce folds in ONE `commit_batch` (ADR-0003, ADR-0031), and the trait default only replays the ops because every real engine overrides it with a native atomic unit (fjall `Batch`, redb one write txn). Over FeOxDB the default is all there is, so a crash lands half a document.

## 4. The surface, if the three were fixed

`range_query(start, end, limit) -> Result<Vec<(Vec<u8>, Vec<u8>)>>` (`range.rs:45`) makes **both bounds inclusive** (`:16`) and materializes the answer. okm's `scan_range(begin, Some(end))` takes an **exclusive** end — usually the byte that follows a prefix — which cannot be turned into an inclusive bound without over-scanning and dropping the tail. The iterator is forward-only (`lower_bound` + `next()`), where okm's contract tests walk every arm backwards (`scan_range_test.rs`, `.rev()`). Both are shimmable by materializing into `ScanIter::Buffered`, which is exactly the buffered scan ADR-0072 refused: a `LIMIT 10` over a large range would pay for the range.

`insert` also resolves conflicts by version timestamp, so a write carrying an older stamp is refused (`OlderTimestamp`) — okm's `put` is an overwrite and must not fail because a wall clock went backwards.

## 5. Why it is not the ephemeral engine either

ADR-0072 already ruled on this mechanism by measurement: a third-party ordered container is generic over `K`/`V`, so a variable-length key is an owned `Vec<u8>` (109 B/entry on an index face), and its range iterator borrows the container so it cannot live inside the `'static` `ScanIter` — the only way in is to materialize, the memory spike the store exists to avoid. The skip list was landed and replaced there: 48 B/entry against the packed B+tree's 31 on the same face.

FeOxDB is that shape with a page-sized record on top: 3,916 B/entry where the rejected skip list measured 48. It also inherits the empty-value refusal in memory mode (same `insert` path), so index faces need a placeholder byte in either role. And its 4 GB default ceiling means roughly a million entries before `OutOfMemory` — okm's test matrix creates a store per case.

## 6. Decision

FeOxDB is not an okm backend, in either mode. It is not scheduled, not kept warm as a candidate, and not listed among the engines; `VirtualStorage` is unchanged. The capabilities it does have — sub-microsecond reads, TTL, CAS, JSON patches, an in-memory cache — are a cache layer's, and a host that wants one can put it beside okm rather than under it.

**Reopen gate:** all four capabilities, plus a consumer. The engine would need a multi-key atomic commit, a durability mode where `Ok` means on disk, empty values, and a lazy exclusive-end range that can also walk backwards; and some caller would need an engine okm does not already carry. A benchmark reopens none of it: the refusal is semantic, and its numbers are already the worse side of every column.

## 7. Refused alternatives

- **Store a placeholder byte for empty values.** Makes okm's encoding bend to an engine's refusal, and puts the difference in the read path forever (ADR-0011 keeps composition single-point).
- **`flush()` after every write.** Turns 1.2–1.6 µs inserts into fsync-bound ones and still leaves §3.3: one flush per op is not one commit per document.
- **One flush per `commit_batch`.** Closer, but a crash inside the flush leaves a prefix of the batch; atomicity is the engine's promise or it is nobody's.
- **Use it only as the non-persistent engine.** 157× and 29× the density cost of the engine that exists for that mode, 10× its write latency, and empty values still refused.
- **Keep it as "recorded, not designed".** It was measured; leaving it open implies the answer is pending, and the answer is three contract violations and two order-of-magnitude gaps.