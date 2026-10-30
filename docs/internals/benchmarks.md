# Baseline Benchmarks

Baseline numbers for the core paths (PLAN Phase 7 "Benchmarks"), recorded 2026-09-12 on the initial criterion baseline (`--save-baseline initial`). Purpose: relative comparison across engine/codec changes — **not** absolute truth. Shared-runner variance is ±5%; no CI regression gates until a dedicated runner exists.

Re-run: `cargo bench -p okm-core --bench core_paths` (add `-- --baseline initial` to compare against this table). Engine-specific benches (fjall/slatedb/remote round trip) land as separate bench files when wired.

## Environment

- dev profile (criterion builds benches in release by default — numbers
are release-grade), local runner
- MockStore is gone; baseline engine = TestStore default (slatedb
in-memory) — memory-locality representatives, not disk-engine representatives

## Declarations under test

- `BenchKey { org_id: u32, user_id: u64, tag: [u8; 4] }` — 16-byte key
- `BenchRow { level: u32, score: Reverse<u64>, visits: VarInt<u64>, name: String }`
— hot 6B + two cold TLV frames, one field index (by_level) + one reduce (count+sum per level)

## Key / payload encoding

| path | mean |
|---|---|
| key encode (16B, 3 fields) | 5.5 ns |
| key decode | 3.0 ns |
| encode_prefix_named (full 3 fields) | 7.7 ns |
| encode_prefix_named (first 2) | 7.0 ns |
| payload encode (hot 6B + 2 cold frames) | 92.4 ns |
| payload decode | 11.8 ns |
| Reverse\<u64\> bit-flip | 4.9 ns |

Readings: pure pointer-slicing decode is ~2× faster than encode (no allocation). Payload encode is dominated by the two TLV frames + String heap copy; hot-segment arithmetic is sub-ns.

## Index scan + fetch-back (per prefix value)

Recorded 2026-09-12:

| fanout | scan_index + fetch-back | scan_covered |
|---|---|---|
| 1 | 124 ns | 85 ns |
| 100 | 11.1 µs (~111 ns/row) | — |
| 10,000 | 1.29 ms (~129 ns/row) | — |

Linear in fanout (~110-130 ns/row: key decode + payload point-read per hit). scan_covered at fanout 1 saved the fetch-back read (~30%).

Re-run 2026-10-24, after `includes` gained its read side and both doors became comparable at every fanout (the bench index is now `by_level { fields(level), includes(score) }`, so the covered scan really decodes an 8-byte field instead of an empty segment):

| fanout | scan (fetch-back) | scan_covered | ratio |
|---|---|---|---|
| 1 | 11.0 µs | 6.4 µs | 1.7× |
| 100 | 515 µs (5.2 µs/row) | 104 µs (1.0 µs/row) | 4.9× |
| 10,000 | 60.8 ms (6.1 µs/row) | 10.7 ms (1.1 µs/row) | 5.7× |

The absolute values do not line up with the 2026-09-12 block (runner and engine version both moved), so compare ratios within a block, never across. What the ratio says: one fetch-back hit costs ~5 µs more than a covered hit, so a high-fanout read is ~5× faster through the covered door — that gap is the whole reason `includes` exists, and the reason it is opt-in (every row write pays for the copied bytes).

## Write path (in-memory engine)

| path | mean |
|---|---|
| put (row + index entry + reduce fold) | 1.31 µs |
| point get | 42 ns |
| batch commit (100 ops) | 4.1 µs (~41 ns/op) |

The full semantic put (primary + index + reduce RMW) costs ~1.3 µs on an in-memory map — the reduce's read-modify-write and the index entry are the dominant parts, not the primary write. Raw batch ops commit at ~41 ns/op; the gap to a semantic put is index/reduce maintenance.

## The same rows in four places: OKM+fjall, bare fjall, Redis, Parquet

`cargo run -p okm-core --release --example engine_paths --features test-engines,fjall` — one machine, one thread, **1,000,000 rows** of the shape used throughout this document (`BenchKey` + `BenchRow`: one index, one reduce).

Writes are one timed pass: a second pass would overwrite, and an overwrite in an LSM is a tombstone plus a reduce unfold, several times slower than an insert — a median over three runs would have measured that instead of the write. Reads and scans are the median of three. The scan probe is one index value holding a hundredth of the table (`level = i % 100`), so 10,000 rows behind one value.

**Bare fjall** is the arm that matters here: the same index value and the same documents, split into two keyspaces, iterated with fjall's own iterator — no okm in between, so no key struct, no document decode, no reduce, no `VirtualStorage` hop. It answers "can an LSM do a batch scan", and it says where okm's scan spends its time.

The Redis arm spoke to valkey 9.1.2 over loopback through a RESP client written into the example, so no crate joins `okm-core` for it; `REDIS_URL` points it elsewhere and it skips itself when nothing answers.

The mapping, because it is what makes the columns comparable: an OKM **put** is the row, its index entry and its reduce fold in one call; bare fjall's put is two writes, document and index entry, with no reduce; in Redis it is `SET` + `SADD` + two `HINCRBY` inside `MULTI`/`EXEC`. A **batch** is one `Batch::commit` against one `MULTI`+`EXEC`. A **scan** is one index value with 10,000 rows behind it, with the row fetched and without it (`scan_covered` / `SMEMBERS` with the score in the member). Parquet has no counterpart to any of the point operations, so those cells are empty.

| arm | put | get | batch (100 rows) | scan (10,000 rows) | covered | on disk / in server |
|---|---|---|---|---|---|---|
| okm + fjall (Buffer mode) | 4.8 µs | 1.2 µs | 0.97 µs/row | 10.1 ms (1.01 µs/row) | 5.0 ms (0.50 µs/row) | 208 MB |
| bare fjall (no okm) | 1.7 µs | 782 ns | — | 4.4 ms (0.44 µs/row) | 1.2 ms (0.12 µs/row) | 208 MB |
| parquet (zstd 1) | 0.12 µs | — | — | 25.4 ms, full pass (25 ns/row) | — | 3.3 MB |
| redis / valkey (loopback) | 22 µs | 8.4 µs | 3.1 µs/row | 4.4 ms (0.44 µs/row) | 1.2 ms (0.12 µs/row) | 171 MB |

How to read it:

- **Point lookups are what this store is for, and at this size fjall is an order of magnitude ahead**: 777 ns bare, 1.1 µs through okm, 8.3 µs on Redis. The gap is not memory against disk — by the storage hierarchy in the wiki, that difference is one order of magnitude at most, and zero when the working set sits in the page cache — it is one round trip, which every Redis read pays.
- **Batch scanning is not the core scenario, and it belongs to Parquet anyway**: okm exports Parquet (Iceberg after that), at 3.4 bytes per row and 25 ns per row for a full pass. Judging a store by "scan a hundred rows, scan ten thousand" is analytic work pushed into an OLTP engine.
- **And in the case Redis is supposed to win — one set, every member in one round trip — it does not win much.** Against bare fjall: fanout 100 is 109 ns per row for the LSM against 192 for Redis; 1,000 rows is 102 against 121; 10,000 is 132 against 116; 100,000 is 196 against 136. Redis's advantage appears past ten thousand rows and tops out at 1.4x, while below a thousand the LSM is faster.
- **okm's own scan is slower than both** (490 ns per row at fanout 100, 4.6x bare fjall), and that difference is the model layer, not the engine. Where it goes, measured: one redundant copy (the suffix of each entry re-allocated so the prefix could be dropped) was worth 26 ns per row in isolation — 151 ns against 125 ns over the same 10,000 entries, now drained instead of copied — and the rest is `ScanIter` handing back an owned `(Vec, Vec)` per row plus the typed entry decode. Removing those copies is a `VirtualStorage` trait change, and it is not worth it for a scenario whose real home is Parquet. Charging any of this to "disk is slow" is the misattribution the wiki argues against.
- **Writes: 1.7 µs bare, 22 µs on Redis.** Nothing in those 22 µs is the data structure; it is round trips and protocol parsing.
- **The durability knob is worth 76x**: the same fjall, same put, 4.8 µs buffered against 0.37 ms with an fsync per write.
- **Footprints**: fjall's LSM files at 208 MB (218 B/row; 130–208 MB across runs depending on where compaction stood), Redis's 171 MB of `used_memory`, Parquet's 3.3 MB. None is comparable with the series log's 56 B/record — a KV store with an index and a reduce is a different job.

### One index value, covered: okm's scan, bare fjall, and Redis

| fanout | okm scan | bare fjall | redis | bare fjall ÷ redis |
|---|---|---|---|---|
| 1 | 7.0 µs | 842 ns | 7.8 µs | 0.1x |
| 10 | 1.1 µs | 144 ns | 879 ns | 0.2x |
| 100 | 490 ns | 107 ns | 197 ns | 0.5x |
| 1,000 | 452 ns | 102 ns | 128 ns | 0.8x |
| 10,000 | 495 ns | 124 ns | 177 ns | 0.7x |
| 100,000 | 356 ns | 188 ns | 142 ns | 1.3x |

Cost per row, no row reads, one run.

- **Redis's "batching advantage" only exists against okm's model layer.** Against the bare LSM it is slower below a thousand rows and at best 1.3x faster at a hundred thousand — in its best shape (one set, one `SMEMBERS`, no decoding), that is the whole win.
- **Bare fjall is 100–190 ns per row and nearly flat**; the 356–495 ns okm shows is about 4x model layer (the engine iterator's owned pair per row, the typed decode, the result vector), not engine.
- **At fanout 1 none of the three is cheap** (6.7 µs / 952 ns / 8.0 µs): opening a range, decoding one entry and building a result set is fixed cost. Asking for one row should be a primary-key read — the get column: 1.1 µs okm, 777 ns bare, 8.3 µs Redis.
- **A covered row costs less than half a point get** (500 ns against 1.2 µs through okm, and a sixth of one bare): the index half was never the expensive half; the row reads are, and a batch scenario should avoid them or hand them to Parquet.
- **So**: point lookups lose to Redis by an order of magnitude, batch scans win by 1.1–1.4x at best, and batch scans are not what a store is for. That is the wiki's critique measured — Redis's problem is not that in-memory is bad, it is that it is ahead in neither direction and still costs a network hop and a process to operate.

## Dynamic codec tax (okm-dynamic vs Rust derive, same declaration)

| path | dynamic | derive | tax |
|---|---|---|---|
| key encode | 40.0 ns | 5.1 ns | ~7.8× |
| payload encode | 84.2 ns | 107.7 ns | ~0.8× (dynamic was faster here) |

The interpreter tax on keys is real but absolute-scale tiny (40 ns vs 5 ns — both far below any engine op). The payload reversal (dynamic faster than derive) indicates the derive's String TLV path allocates more than the schema walk — worth an investigate later, not a blocker. Dynamic decode benchmarks land with the PyO3 binding work.

## The change log's capture tax

`cargo run --release --example changelog_tax -p okm-core --features test-engines,fjall` (9 paired rounds × 5,000 puts, order flipped each round, median of the per-round ratios — two fjall stores compact on their own schedule, so an unpaired median drifts):

| path | mean |
|---|---|
| put, no log | 1.32–1.55 µs |
| put, captured | 1.37–1.71 µs |
| capture tax | **+84 to +99 ns/write, +6.9 to +8.5%** (one outlier run: +54 ns, +5.4%) |

Six runs on this machine: +54/+5.4%, +84/+7.2%, +95/+8.5%, +99/+6.9%, +132/+4.7%, and one unpaired early run that printed a NEGATIVE tax (that is what the paired rounds fixed). Read it as **~+50 to +135 ns/write, ~5 to 8%**, call it ~90 ns.

The tax is the encode of the frame plus a memcpy into the segment buffer: at 16 B keys and a ~45 B payload it is well under a tenth of a durable put, and it does not touch the store's own cost. The spread is the machine, not the code — the two arms are two fjall stores compacting on their own schedule, which is why the numbers come from paired rounds with the order flipped each time.

Measured against `fjall_tmp`, a durable store — which is the honest arm for a capture tax. The in-memory engine was NOT usable for this until now: `SlatedbSync` awaited `WriteHandle::await_durable` on every `put`, and durability waits for the next WAL flush, whose slatedb default is **100 ms** — measured 101 ms per put, flat, independent of store size (300 puts = 30.3 s), which is what made `cargo bench … write-changelog` look like a hang. The in-memory engine now skips the wait (the object store dies with the process: there is nothing to recover), and the same loop runs in 5 ms (**~17 µs/put**, 6,000x).

## Context: versus a network KV service (Redis, public data)

Public redis-benchmark data (official docs, same-AZ r7g/c6i measurements): a GET/SET is **~100-300 µs p50** (loopback or same-AZ TCP; single core ~145K ops/s, pipelined ~1.5-1.8M ops/s). The breakdown is consistent across published analyses: hash-table lookup ~50-100 ns, everything else — RESP parse/serialize, kernel syscalls, network round-trip — is ~97% of the operation. Value size scales the network side linearly (1 MB value → ~12 ms).

OKM is an **in-process library** — the same comparison class as the "in-process cache" numbers (31 ns hash-lookup + pointer), not the network-service class. Per-operation costs from the baseline above sit exactly where an in-process store should:

| operation | Redis (network, public data) | OKM (in-process, baseline) |
|---|---|---|
| point get | ~100-300 µs | 42 ns (put+get path) / ~130 ns with index fetch-back |
| point put | ~150-300 µs | 1.3 µs (semantic: primary + index entry + reduce fold) |
| raw batch commit | pipelined ~0.6 µs/op (16-deep) | ~41 ns/op |
| scan a prefix value's entries | N round trips (or SCAN iteration) | ~120 ns/row, one call |

Reading the table honestly:

- The 3-4 order-of-magnitude gap is the **network**, not server quality —
Redis's own hash lookup is 50-100 ns, the same class as OKM's map ops. Comparing Redis's *total* latency to OKM's *core* latency compares different segments of the stack.
- What OKM **loses** to Redis — nothing structural, because the
comparison unit is wrong. Redis is always "cache service + the application serving API on top"; Krystallizer-style services are exactly that app, so the honest unit is **OKM+service vs Redis+service**. The "multi-client / shared-state / TTL" list lives in the service layer either way: concurrent access is the service's connection plane (WS handlers, the same concurrency regardless of storage), shared state across instances is a deployment concern (OKM hosts on shared engines or sharded instances, ADR-0010 §5's bare/hosted forms are the vocabulary), and TTL/expiry is application logic Redis never implemented for you either. What the application gains modeling on OKM instead of Redis patterns: semantic writes (one put = primary + index + reduce, vs N round trips + client-side bookkeeping), schema'd bytes (no serialize/deserialize per hop), and ordered structures (prefix scans) without ZSET workarounds.
- Deployment topology: Redis's is fixed (network server, its
persistence, its eviction policy); OKM's is a free composition — in-process engine (this baseline), sharded bare hosts (one engine per shard, orchestrator-routed), hosted multi-tenant segments (one engine, `NestStorage` (with `#[ok_ns]`) executors), or a Redis-like network service (RemoteStore behind the service's connection plane — the WS-CHANNEL integration) — each form is a constructor call away, and the byte format is unchanged across all of them.
- Throughput: Redis single-core ~145K ops/s (I/O bound); OKM single
thread ~700K semantic puts/s (1.3 µs each). The honest statement: for in-process Booth storage (OKM's design point), the network service's latency floor is a cost you delete — and if a network surface is needed, wrapping a host in the service's connection plane rebuilds the Redis shape without the Redis patterns.

## The series log's write path (`okm-series`)

`cargo run -p okm-series --release --example series_tax` — 50,000 records into one part of 64 KiB blocks, one bucket, three shapes of body (256 distinct bodies for the first two, one per record for the third). Bytes are deterministic; times are single-run figures, and re-running them on a loaded machine moves them by up to a factor of two (the same shape measured 872, 1,083, 1,494 and 1,826 ns/record across four runs of the same binary). Read the ORDER of the columns, not the digits. Each append now also reads the log's clock, because the caller cannot supply a time (ADR-0046 §2).

| workload | shape | bytes | B/rec | x smaller than raw | write ns/rec | read M rec/s |
| --- | --- | --- | --- | --- | --- | --- |
| repetitive text | no compression, dictionary on | 1,419,466 | 28.4 | 3.3x | 347 | 9.3 |
| repetitive text | no compression, dictionary off | 4,984,033 | 99.7 | 0.9x | 862 | 8.8 |
| repetitive text | zstd 1, dictionary on | 64,517 | 1.3 | 73x | 357 | 15.6 |
| repetitive text | zstd 1, dictionary off | 249,694 | 5.0 | 18.9x | 812 | 15.2 |
| repetitive text | zstd 3, dictionary on | 58,437 | 1.2 | 81x | 348 | 17.3 |
| identity heavy | no compression, dictionary on | 1,302,483 | 26.0 | 1.4x | 353 | 8.8 |
| identity heavy | no compression, dictionary off | 2,062,171 | 41.2 | 0.9x | 386 | 8.5 |
| identity heavy | zstd 1, dictionary on | 29,136 | 0.6 | 62x | 348 | 7.2 |
| identity heavy | zstd 1, dictionary off | 104,812 | 2.1 | 17.3x | 363 | 7.3 |
| identity heavy | zstd 3, dictionary on | 29,094 | 0.6 | 62x | 318 | 10.2 |
| unique id per record | no compression, dictionary on | 3,340,331 | 66.8 | 1.1x | 857 | 8.1 |
| unique id per record | no compression, dictionary off | 4,090,999 | 81.8 | 0.9x | 894 | 5.6 |
| unique id per record | zstd 1, dictionary on | 251,179 | 5.0 | 15.3x | 872 | 7.4 |
| unique id per record | zstd 1, dictionary off | 259,980 | 5.2 | 14.8x | 683 | 6.3 |
| unique id per record | zstd 3, dictionary on | 250,453 | 5.0 | 15.3x | 859 | 5.6 |

Block size, same 50,000 unique-id records, zstd 1, dictionary on:

| block | write ns/record | blocks on disk | bytes |
| --- | --- | --- | --- |
| 16 KiB | 2,267 | 235 | 276,179 |
| 64 KiB | 942 | 59 | 251,179 |
| 256 KiB | 556 | 15 | 245,302 |

How to read it:

- **Compression is free at 64 KiB blocks, and that is why it is on by default.** A block
is compressed once per ~1,000 records, so the codec's cost sits inside the run-to-run noise; on the two shapes that compress well the compressed path is even faster than the uncompressed one, because the write syscall carries 20–70x fewer bytes.
- **Block size is worth more than compression level.** The per-block work (compress,
write, fsync, index row) dominates the per-record work: 16 KiB blocks cost 2.3 µs per record and 256 KiB blocks cost 0.56 µs. Level 3 buys 9% of the bytes on highly repetitive data and nothing measurable elsewhere.
- **The index is not fsynced per block, deliberately** — it is derived data and adoption
rebuilds it. Dropping that second sync took the write path from 1,345 to 882 ns/record at 64 KiB blocks.
- **The dictionary and the codec are not interchangeable.** On closed-set values interning
removes repetition before the codec sees it: 4x fewer bytes on top of compression, and no write-time cost (872 against 683 ns/record is the counter-case). With one unique id per record it buys 3% of the bytes for ~15% of the write time — that is the shape `Config::dict = None` is for.
- **Reads are 5–17 M records/s**, one block decompressed at a time and nothing cached
between calls; the spread tracks how much work each block's decompression is, not how many records it holds.
- Single-threaded, XFS on NVMe (`/tmp` here is not tmpfs, so the fsync per block is real),
no concurrent readers, 59 blocks per run.

## One million log records: okm-series against Parquet

`cargo run -p okm-series --release --example log_vs_parquet` — the same million records written twice, once into a log and once into a Parquet file (arrow 60, ZSTD level 1, dictionary encoding on, the defaults on both sides). The record is a realistic log line: level, service, host, a message with an id and a latency in it, a 32-hex trace id, a latency, a status, a tenant. Two of the eight fields are unique per record and three are closed sets of 5/8/16 values.

The test prints a baseline first — 195 ns/record to generate the record, 302 ns with the `Body` built — so neither side is charged for the harness's own arithmetic.

| writer | bytes | B/record | MB/s | write ns/record |
|---|---|---|---|---|
| okm-series, zstd 1, dictionary | 56,087,068 | 56.1 | 28.4 | 1,881 |
| okm-series, zstd 1, no dictionary | 60,123,961 | 60.1 | 33.7 | 1,703 |
| okm-series, raw (no codec, no dictionary) | 165,774,882 | 165.8 | 66.6 | 2,374 |
| parquet, 65,536-row groups | 44,819,259 | 44.8 | 65.8 | 650 |
| parquet, 4,096-row groups | 48,393,042 | 48.4 | 68.3 | 676 |

| reader | full scan | one record |
|---|---|---|
| okm-series | 2.64 M records/s | 68 µs (`read(offset, 1)`) |
| parquet, 65,536-row groups | 9.84 M records/s | 6,618 µs |
| parquet, 4,096-row groups | 8.39 M records/s | 887 µs |

How to read it:

- **Parquet is 20% smaller and ~3x faster to write.** Columnar encoding plus dictionary
encoding per column is simply a better answer for bytes than interleaved records, and the writer batches a row group at a time instead of compressing every 64 KiB.
- **The log is the only one of the two that can fetch one record by address.** 68 µs
against 887 µs is not a tuning difference: Parquet has no way to reach row *n* without decoding the columns of its row group, so a 4,096-row group is the *cheapest* shape for this and a 65,536-row group costs 6.3 ms. The log seeks by offset, decompresses one block and stops.
- **The full-scan numbers are not like for like.** The Parquet scan hands back Arrow
arrays; the log materialises every record as a `Body` with eight values and five owned strings. A caller that only wants one column should be reading Parquet.
- **Block size is the knob between the two.** 64 KiB blocks make a point read decompress
64 KiB (~45 of the 68 µs); 256 KiB blocks would cut the write cost and multiply the point-read cost. The 23 µs that is not decompression is the part index being re-parsed on every call (twice: once to order the parts, once to read them).
- **The dictionary barely pays at this shape, and after the fix below it no longer
costs.** 7% of the bytes for 8% of the write time: two of eight fields are unique per record, so the tiers keep them inline and the dictionary only removes 29 distinct strings. It is the closed-set shape that pays, as the table in the section above shows.

### Two defects this test found

Both were invisible at 50,000 records and both were fixed in the run that produced the numbers above.

1. **The part index was read one row per `read` syscall.** An unbuffered `File` costs
~200 ns per call whatever the size, and a million-record part has 2,214 blocks, so every point read paid 429 µs of syscalls before touching a record. Reading the whole index once and slicing it took `read(offset, 1)` from 472 µs to 67 µs (and `parts()` from 429 µs to 23 µs).
2. **The dictionary's frequency table was keyed by an owned `String`.** Every string that
was not already interned allocated a copy just to count it, so a field that is unique per record — a trace id — filled the table with strings that could never reach the second occurrence the tiers demand. On this shape that was two million live `String`s and 1,077 ns per record (2,859 → 1,862 after keying the table by the string's 64-bit hash, with the table capped at 2^20 tracked strings). A hash collision can only promote a string that occurred once; it cannot resolve to the wrong text, because the block and the `promoted` map are keyed by the string.

## Not yet benched

- fjall / slatedb engine paths (feature benches)
- remote round trip (framed put through okm-wire + NestStorage pump)
— blocked on the WS/transport integration landing
- reduce fold at large group cardinality (current table: single group)
