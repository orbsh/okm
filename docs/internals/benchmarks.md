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
- MockStore (BTreeMap) — engine numbers are memory-locality
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

| fanout | scan_index + fetch-back | scan_covered |
|---|---|---|
| 1 | 124 ns | 85 ns |
| 100 | 11.1 µs (~111 ns/row) | — |
| 10,000 | 1.29 ms (~129 ns/row) | — |

Linear in fanout (~110-130 ns/row: key decode + payload point-read per
hit). scan_covered at fanout 1 saves the fetch-back read (~30%).

## Write path (MockStore)

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
- What OKM **loses** to Redis: multi-client concurrent access, network
  shared-state semantics, TTL/persistence machinery. What it **wins**:
  per-operation latency floor, semantic writes (one put = primary + index
  + reduce, vs N round trips to Redis), and schema'd bytes (no
  serialize/deserialize per hop — values are already encoded at the
  boundary).
- Throughput: Redis single-core ~145K ops/s (I/O bound); OKM single
  thread ~700K semantic puts/s (1.3 µs each) — but the two serve
  different deployment topologies. The honest statement: **for
  in-process Actor storage (OKM's design point), the network service's
  latency floor is the cost you delete.**

## Not yet benched

- fjall / slatedb engine paths (feature benches)
- remote round trip (framed put through okm-wire + StorageHost pump)
  — blocked on the WS/transport integration landing
- reduce fold at large group cardinality (current table: single group)
