# Baseline Benchmarks

Baseline numbers for the core paths (PLAN Phase 7 "Benchmarks"), recorded
2026-09-12 on the initial criterion baseline (`--save-baseline initial`).
Purpose: relative comparison across engine/codec changes — **not** absolute
truth. Shared-runner variance is ±5%; no CI regression gates until a
dedicated runner exists.

Re-run: `cargo bench -p okm-core --bench core_paths` (add
`-- --baseline initial` to compare against this table). Engine-specific
benches (fjall/slatedb/remote round trip) land as separate bench files
when wired.

## Environment

- dev profile (criterion builds benches in release by default — numbers
  are release-grade), local runner
- MockStore is gone; baseline engine = TestStore default (slatedb
  in-memory) — memory-locality
  representatives, not disk-engine representatives

## Declarations under test

- `BenchKey { org_id: u32, user_id: u64, tag: [u8; 4] }` — 16-byte key
- `BenchRow { level: u32, score: Reverse<u64>, visits: VarInt<u64>, name: String }`
  — hot 6B + two cold TLV frames, one field index (by_level) + one reduce
  (count+sum per level)

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

Readings: pure pointer-slicing decode is ~2× faster than encode (no
allocation). Payload encode is dominated by the two TLV frames + String
heap copy; hot-segment arithmetic is sub-ns.

## Index scan + fetch-back (per prefix value)

Recorded 2026-09-12:

| fanout | scan_index + fetch-back | scan_covered |
|---|---|---|
| 1 | 124 ns | 85 ns |
| 100 | 11.1 µs (~111 ns/row) | — |
| 10,000 | 1.29 ms (~129 ns/row) | — |

Linear in fanout (~110-130 ns/row: key decode + payload point-read per
hit). scan_covered at fanout 1 saved the fetch-back read (~30%).

Re-run 2026-10-24, after `includes` gained its read side and both doors became
comparable at every fanout (the bench index is now
`by_level { fields(level), includes(score) }`, so the covered scan really decodes
an 8-byte field instead of an empty segment):

| fanout | scan (fetch-back) | scan_covered | ratio |
|---|---|---|---|
| 1 | 11.0 µs | 6.4 µs | 1.7× |
| 100 | 515 µs (5.2 µs/row) | 104 µs (1.0 µs/row) | 4.9× |
| 10,000 | 60.8 ms (6.1 µs/row) | 10.7 ms (1.1 µs/row) | 5.7× |

The absolute values do not line up with the 2026-09-12 block (runner and engine
version both moved), so compare ratios within a block, never across. What the
ratio says: one fetch-back hit costs ~5 µs more than a covered hit, so a
high-fanout read is ~5× faster through the covered door — that gap is the whole
reason `includes` exists, and the reason it is opt-in (every row write pays for
the copied bytes).

## Write path (in-memory engine)

| path | mean |
|---|---|
| put (row + index entry + reduce fold) | 1.31 µs |
| point get | 42 ns |
| batch commit (100 ops) | 4.1 µs (~41 ns/op) |

The full semantic put (primary + index + reduce RMW) costs ~1.3 µs on an
in-memory map — the reduce's read-modify-write and the index entry are
the dominant parts, not the primary write. Raw batch ops commit at
~41 ns/op; the gap to a semantic put is index/reduce maintenance.

## Dynamic codec tax (okm-dynamic vs Rust derive, same declaration)

| path | dynamic | derive | tax |
|---|---|---|---|
| key encode | 40.0 ns | 5.1 ns | ~7.8× |
| payload encode | 84.2 ns | 107.7 ns | ~0.8× (dynamic was faster here) |

The interpreter tax on keys is real but absolute-scale tiny (40 ns vs
5 ns — both far below any engine op). The payload reversal (dynamic
faster than derive) indicates the derive's String TLV path allocates
more than the schema walk — worth an investigate later, not a blocker.
Dynamic decode benchmarks land with the PyO3 binding work.

## The change log's capture tax

`cargo run --release --example changelog_tax -p okm-core --features test-engines,fjall`
(9 paired rounds × 5,000 puts, order flipped each round, median of the per-round
ratios — two fjall stores compact on their own schedule, so an unpaired median drifts):

| path | mean |
|---|---|
| put, no log | 1.32–1.55 µs |
| put, captured | 1.37–1.71 µs |
| capture tax | **+84 to +99 ns/write, +6.9 to +8.5%** (one outlier run: +54 ns, +5.4%) |

Six runs on this machine: +54/+5.4%, +84/+7.2%, +95/+8.5%, +99/+6.9%, +132/+4.7%,
and one unpaired early run that printed a NEGATIVE tax (that is what the paired
rounds fixed). Read it as **~+50 to +135 ns/write, ~5 to 8%**, call it ~90 ns.

The tax is the encode of the frame plus a memcpy into the segment buffer: at 16 B
keys and a ~45 B payload it is well under a tenth of a durable put, and it does not
touch the store's own cost. The spread is the machine, not the code — the two arms
are two fjall stores compacting on their own schedule, which is why the numbers come
from paired rounds with the order flipped each time.

Measured against `fjall_tmp`, a durable store — which is the honest arm for a
capture tax. The in-memory engine was NOT usable for this until now: `SlatedbSync`
awaited `WriteHandle::await_durable` on every `put`, and durability waits for the
next WAL flush, whose slatedb default is **100 ms** — measured 101 ms per put, flat,
independent of store size (300 puts = 30.3 s), which is what made `cargo bench …
write-changelog` look like a hang. The in-memory engine now skips the wait (the
object store dies with the process: there is nothing to recover), and the same loop
runs in 5 ms (**~17 µs/put**, 6,000x).

## Context: versus a network KV service (Redis, public data)

Public redis-benchmark data (official docs, same-AZ r7g/c6i measurements):
a GET/SET is **~100-300 µs p50** (loopback or same-AZ TCP; single core
~145K ops/s, pipelined ~1.5-1.8M ops/s). The breakdown is consistent
across published analyses: hash-table lookup ~50-100 ns, everything else
— RESP parse/serialize, kernel syscalls, network round-trip — is ~97%
of the operation. Value size scales the network side linearly (1 MB
value → ~12 ms).

OKM is an **in-process library** — the same comparison class as the
"in-process cache" numbers (31 ns hash-lookup + pointer), not the
network-service class. Per-operation costs from the baseline above
sit exactly where an in-process store should:

| operation | Redis (network, public data) | OKM (in-process, baseline) |
|---|---|---|
| point get | ~100-300 µs | 42 ns (put+get path) / ~130 ns with index fetch-back |
| point put | ~150-300 µs | 1.3 µs (semantic: primary + index entry + reduce fold) |
| raw batch commit | pipelined ~0.6 µs/op (16-deep) | ~41 ns/op |
| scan a prefix value's entries | N round trips (or SCAN iteration) | ~120 ns/row, one call |

Reading the table honestly:

- The 3-4 order-of-magnitude gap is the **network**, not server quality —
  Redis's own hash lookup is 50-100 ns, the same class as OKM's map ops.
  Comparing Redis's *total* latency to OKM's *core* latency compares
  different segments of the stack.
- What OKM **loses** to Redis — nothing structural, because the
  comparison unit is wrong. Redis is always "cache service + the
  application serving API on top"; Krystallizer-style services are
  exactly that app, so the honest unit is **OKM+service vs
  Redis+service**. The "multi-client / shared-state / TTL" list lives
  in the service layer either way: concurrent access is the service's
  connection plane (WS handlers, the same concurrency regardless of
  storage), shared state across instances is a deployment concern
  (OKM hosts on shared engines or sharded instances, ADR-0010 §5's
  bare/hosted forms are the vocabulary), and TTL/expiry is application
  logic Redis never implemented for you either. What the application
  gains modeling on OKM instead of Redis patterns: semantic writes (one
  put = primary + index + reduce, vs N round trips + client-side
  bookkeeping), schema'd bytes (no serialize/deserialize per hop), and
  ordered structures (prefix scans) without ZSET workarounds.
- Deployment topology: Redis's is fixed (network server, its
  persistence, its eviction policy); OKM's is a free composition —
  in-process engine (this baseline), sharded bare hosts (one engine per
  shard, orchestrator-routed), hosted multi-tenant segments (one engine,
  `NestStorage` (with `#[ok_ns]`) executors), or a Redis-like network service (RemoteStore
  behind the service's connection plane — the WS-CHANNEL integration) —
  each form is a constructor call away, and the byte format is unchanged
  across all of them.
- Throughput: Redis single-core ~145K ops/s (I/O bound); OKM single
  thread ~700K semantic puts/s (1.3 µs each). The honest statement: for
  in-process Booth storage (OKM's design point), the network service's
  latency floor is a cost you delete — and if a network surface is
  needed, wrapping a host in the service's connection plane rebuilds
  the Redis shape without the Redis patterns.

## The series log's write path (`okm-series`)

`cargo run -p okm-series --release --example series_tax` — 50,000 records into one part
of 64 KiB blocks, one bucket, three shapes of body (256 distinct bodies for the first
two, one per record for the third). Bytes are deterministic; times are single-run
figures whose run-to-run spread is about ±25%, so read them as magnitudes, not decimals.

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
  is compressed once per ~1,000 records, so the codec's cost sits inside the run-to-run
  noise; on the two shapes that compress well the compressed path is even faster than the
  uncompressed one, because the write syscall carries 20–70x fewer bytes.
- **Block size is worth more than compression level.** The per-block work (compress,
  write, fsync, index row) dominates the per-record work: 16 KiB blocks cost 2.3 µs per
  record and 256 KiB blocks cost 0.56 µs. Level 3 buys 9% of the bytes on highly
  repetitive data and nothing measurable elsewhere.
- **The index is not fsynced per block, deliberately** — it is derived data and adoption
  rebuilds it. Dropping that second sync took the write path from 1,345 to 882 ns/record
  at 64 KiB blocks.
- **The dictionary and the codec are not interchangeable.** On closed-set values interning
  removes repetition before the codec sees it: 4x fewer bytes on top of compression, and
  no write-time cost (872 against 683 ns/record is the counter-case). With one unique id
  per record it buys 3% of the bytes for ~15% of the write time — that is the shape
  `Config::dict = None` is for.
- **Reads are 5–17 M records/s**, one block decompressed at a time and nothing cached
  between calls; the spread tracks how much work each block's decompression is, not how
  many records it holds.
- Single-threaded, XFS on NVMe (`/tmp` here is not tmpfs, so the fsync per block is real),
  no concurrent readers, 59 blocks per run.

## Not yet benched

- fjall / slatedb engine paths (feature benches)
- remote round trip (framed put through okm-wire + NestStorage pump)
  — blocked on the WS/transport integration landing
- reduce fold at large group cardinality (current table: single group)
