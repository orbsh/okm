# 0046 — The append-only log: one bucket per directory, deltas for time, and a dictionary that earns its place

> **Languages:** [English](0046-append-only-log.md) (primary) · [中文](0046-append-only-log.zh-CN.md)

**Status:** Proposed (2026-10-28) — design only; nothing is implemented. **Date:** 2026-10-28

## Context

The need is a log: append a record in time order, read it back by offset or by time, and drop old data without paying per-record deletion. Queues are a layer above it, time series are a use of it. The pressure that makes it its own thing rather than an okm table is retention: a consumed message and an expired sample are both "gone", and in an LSM every one of them leaves a tombstone behind, so the engine spends its compaction budget on a table whose only visible effect is shrinkage (aura's `MqData` is exactly this shape: append with a `HighWater` seq, delete below the slowest cursor).

Four facts decide the shape.

- **Wall-clock time has to survive with a declared precision.** A queue needs
order, a series needs the instant. Storing a `u64` nanosecond stamp per record costs 8 bytes on every line for information that is almost always a small step from the previous one.
- **Retention is by time, so the FILE must be the retention unit.** Anything that
drops "old rows" row by row reproduces the problem the log exists to avoid.
- **The payload is nTLV, not opaque bytes.** A generic broker stores bytes and
lets every consumer invent a schema; that freedom is worth nothing in 99% of streams and costs them a decoder each. nTLV is a small self-describing encoding, and converting it to anything else is a walk over items.
- **A string dictionary is a bad default and a good exception.** For a stream of
events whose field names and values are one of twenty shapes, interning is a large win; for a stream whose keys are unique per record (ids, request paths, user agents), a dictionary is a pure loss that grows without bound. The encoding has to be able to go both ways **in the same file, decided per key by evidence**.

## Decision

### 1. The bucket: hours on the wall clock, a directory each

A log is a directory. Its `log.meta` declares three things that never change: `epoch` (Unix ms at creation, for humans and for negative-time refusal), `precision` (s / ms / µs / ns — the quantization unit the writer applies), and `bucket` (seconds per bucket, default 3600).

Bucket index = `floor(unix_seconds / bucket)`. Absolute alignment, not creation-relative: an hour file is the same interval for every log on the machine, which is what makes several logs joinable by name and what makes a shard's directory listing meaningful.

<log>/log.meta <log>/LOCK                      single writer, held for the process lifetime <log>/<bucket>/part-<n>.log     records, append-only, sealed by rename <log>/<bucket>/part-<n>.dict    the strings that earned a code, same lifetime

A part is sealed by renaming `.partial` → the final name (ADR-0045's discipline, reused). A part may rotate early on a size cap; then a bucket holds several parts, and retention is `remove_dir_all(<bucket>)` — one syscall per hour, no tombstones, no scan.

### 2. Time inside a part: a base plus deltas

The part header carries `first_offset` (u64), `base_ts` (full timestamp of the first record in the declared precision), `bucket` (u32, repeated so a file is self-describing), and `record_count` (filled at seal time; 0 while open).

A record is:

[len u32][ts Δ varint][nTLV body]

`ts = previous_ts + Δ`, the first record's Δ being `base_ts`. Deltas are small for a busy stream (one or two bytes at any precision) and honest for a sparse one (a varint grows instead of the file lying about resolution).

**The writer stamps the time; the caller cannot.** `append` takes a body and nothing else. There is no timestamp argument to get wrong, no configuration naming a "time field" that might collide with a real one, and no clock to trust per record: the log reads its own clock at the moment of the write, and the time axis is therefore INGESTION order. A body may carry the application's own time under any tag it likes — that value is data, and it does not touch the record's address.

Two consequences, both deliberate:

- **No backfill.** You cannot import last week's samples into last week's buckets; they
will be stamped now. A log whose time axis is ingestion order cannot also be a warehouse of historical time, and pretending otherwise is how a "one hour per directory" layout starts lying about its own directory names.
- **A clock that steps backwards is clamped, not refused.** NTP steps, a suspended
machine, a container restored from a snapshot: the caller did nothing wrong, so the record is written at the newest timestamp its stream has seen. Two records may share a timestamp, and their offsets still order them.

Monotone time is then a property of the STREAM rather than of a part — a distinction that was a real hole while callers supplied timestamps, because a record an hour earlier lands in an older bucket and opens a fresh part with no history to compare against. The log remembers the newest timestamp each stream has seen, recovered from the parts on open, and uses it as the floor. (The read side gives the timestamp back under a name of its own — the Python binding calls it `_ts` — because a body has tags, not names.)

**There is no per-record checksum, and that is a decision.** Given §3 (nothing is acknowledged before it is fsynced) and the fact that a part is only ever written at its end, the only damage a crash can do is truncate the tail — so the torn record is the one whose `len` runs past the end of the file, and that one rule is the whole recovery procedure. A checksum would not catch more of a crash; it would catch a storage stack that LIED (media bit rot, a network filesystem, an fsync that did not happen), and integrity against that belongs to whoever owns the bytes: the device's ECC, the filesystem's checksums, the object store's own validation. A record format cannot repair a flipped byte anyway — it can only refuse to read it, which turns a data problem into an outage. What the substrate does own is the BOUNDARY: a sealed part carries a whole-part digest in its footer (§8), so a copy, an export or a transport verifies what it carries with one hash instead of one per record.

Reading a part is sequential: "read from offset N" = pick the part, then seek inside it through its block index (§8).

### 3. Durability inverted

ADR-0045 appends AFTER the commit, at most once, and a hole is repaired by rescanning the store. This log has no store behind it: `append` returns an offset only after the bytes are fsynced. The costs differ (one fsync per append unless the caller batches; a `sync` verb for callers that accept a window), the discipline does not: an acknowledged record is on disk.

### 4. nTLV, and where strings go

The item number carries the string's FORM, which is what makes escalation a one-byte change and keeps the hot path branchless:

- `n = 0` — inline: `[len varint][utf8 bytes]`.
- `n = 1` — dictionary #0, body is that dictionary's width (u16).
- `n = 2` — dictionary #1, width u32.
- `n = 3` — dictionary #2, width u64.

Dictionary #k's width is declared in its block header (`u16 << k`), so the body length is derivable and the reader never guesses. The same encoding applies to the tag half and the value half of an item: an nTLV document's *field names* are strings too, and a schemaless stream is exactly the case where they repeat.

### 5. When a string earns a code: tiers of its own width

A dictionary is per part (§6). While a part is open the writer counts `string → (count, id_or_none)` and promotes a string when its count reaches the tier its LENGTH falls in. Bytes are the unit: `L` is the string's byte length, `W` the width of the block it would land in (2, 4 or 8).

| length | promote at | why |
| --- | --- | --- |
| `L <= W` | never | a string no longer than its own code can only grow: one byte of `GET` against a u16 code is a loss at every count |
| `W < L <= 4W` | 10 | the short end: a code saves little per hit |
| `4W < L <= 8W` | 5 | |
| `8W < L <= 16W` | 3 | |
| `16W < L` | 2 | the long end: one more repetition already pays |

The last row is why a long error-log message interning at its second occurrence is not a special case — it is the tier doing its job, and it is also the row that keeps "a fixed 10" from being the whole policy.

The table is configuration, not format: `[[4, 10], [8, 5], [16, 3], [0, 2]]` (multiple of the code width, promotion count; `0` = no upper bound). Changing it changes what FUTURE parts promote and no reader ever needs to know, which is the property a tunable has to have.

Two things the tiers do not express, and both matter:

- **The gate is absolute.** Shorter than its own code is never worth interning, at
any count, in any configuration. This is the one rule that cannot be tuned.
- **Counts are per part, so they restart every hour.** A threshold is therefore paid
again each hour: a message that appears nine times an hour is promoted by the `16W` tier and never by a global constant of 10. This is the strongest argument for the short end being the only conservative one.

The counting table is keyed by the string's 64-bit hash, not by the string, and stops admitting new keys at 2^20. Two reasons, both measured. Holding a copy of every distinct string to count it means a field that is unique per record — a trace id, a message with an id in it — fills the table with strings that can never reach the second occurrence the tiers demand: on a million-record part that was two million live `String`s and 1,077 ns per record. A collision on a hashed key can only promote a string that in fact occurred once, which wastes one code; it can never resolve to the wrong text, because the blocks and the `promoted` map are keyed by the string itself. And a table that only ever grows is a table with no bound at all, so past the cap new text settles as inline — the counters are a policy input, not a record of what happened.

**Why not the exact cross-over.** With `L_+ = L + length field` the arithmetic break-even is `f* = ⌊L_+ / (L_+ − W)⌋ + 1` — for an 8-byte string against a u16 code, `f* = 2`, so the tiers are 5x conservative there and exact at the long end. The trade is deliberate: being early saves `(f − f*) × (L_+ − W)` per later occurrence, being wrong costs one copy of the string inside one hour, and a four-row table is something an operator can reason about while a division with two derived quantities is not. If a part's dictionary turns out to pay at the short end, tighten the list — the knob exists for that, and nothing on disk changes.

Nothing is rewritten when a string is promoted late: earlier records keep their inline copies, which is why a part's dictionary only ever grows and the file stays append-only. A stream of unique keys promotes nothing and stores everything inline: unbounded cardinality is the default path here, not a failure mode.

The counter is not persisted. A part's counts measure a window that dies with the part; a persisted counter would be a second source of truth for a decision whose worst case is one wasted copy.

### 6. Why the dictionary belongs to the part, not the log

A log-global dictionary is the tempting design (one table, extension blocks chained from the header) and it is the wrong one here:

- A part must be **self-contained** — it is the retention unit, the export unit,
and the unit a replication or lake consumer copies. A part that cannot be decoded without a file that outlives it cannot be dropped, archived or shipped alone.
- A log-global dictionary can never be pruned while any part references an entry,
so it needs refcounting or a GC pass — machinery whose failure mode is silent corruption.
- Frequency is local in time. An hour is the right window for "is this string hot",
and a per-part dictionary measures exactly that window.

The cost is duplication: a string repeated every hour is stored once per hour. That is bounded, predictable, and paid only for strings that recur anyway.

### 7. The escalation chain, kept inside the part

Dictionary #0 sits at a fixed offset in `part-<n>.dict` (straight after its header) so the common case needs no indirection. Each block header holds `width`, `count` and `prev_offset`; when a block's id space is exhausted (65,535 at u16) a new block is appended with double the width and its `prev_offset` names the block before it.

The chain is walked BACKWARDS from the newest block, and here is why that matters: the newest block is named by `<bucket>/part-<n>.dict.tail`, a four-byte file replaced by rename. Rename is atomic, so growing the dictionary — the only event that used to require patching a header in place — becomes another append plus a rename. **A part therefore has no in-place mutation at all**, which is what makes §2's claim true: the only thing a crash can damage is the tail of a file, and the only tail that matters is the one being appended to. A reader that meets `n = 3` before any `n = 2` walks `prev_offset` twice. The chain is per part, so "widen" happens at most twice in the life of a file.

### 8. Blocks: compression, and the index that makes a part seekable

A part's body is a sequence of BLOCKS: 64 KiB of records by default, each compressed on its own (zstd level 1–3; lz4 where the CPU matters more than the ratio).

- **Blocks, not records.** A 40-byte record compressed alone is header plus noise —
a zstd frame costs ~13 bytes before it saves anything. The repetition worth compressing lives BETWEEN records (the same field names, the same level values, the same URL shape), and only a block can see it.
- **Every block, not only the long ones.** A size threshold makes two shapes of one
file and still cannot know whether a record would have compressed: the ratio is a property of its neighbours, not of its own length.
- **At flush, not at seal.** Compression happens when the block buffer fills, so the
append path pays it once per 64 KiB instead of once per record, and the file is written once. Compressing at seal would give the codec the whole hour and would turn every seal into a rewrite of the part — 2x write amplification for a few percent of ratio.

**The overlap with the dictionary (§5) is real, and the measurement says it is narrower than the paper argument suggested** (`docs/internals/benchmarks.md`): on values drawn from small closed sets, interning cuts the bytes 4x ON TOP of compression and — because the codec is handed a smaller body — does not cost write time; with one unique id per record it buys 3% of the bytes for ~15% of the write time. So `Config::dict` is a per-log switch that defaults to on, and the deciding question is not "is this text" but "are these values a closed set, or is every record new".

**Compression is on by default (zstd level 1), and the measurement is the reason, not a preference.** At 64 KiB blocks a block is compressed once per ~1,000 records, so the codec's cost is inside the noise of the append path: 872 ns/record with compression and 857 without on the shape that compresses worst, and on the shapes that compress well the compressed path is measurably FASTER (357 against 347 + a 22x smaller file, because the write syscall carries less). Level 3 is not the default: it buys 9% of the bytes on highly repetitive data and nothing elsewhere, and the block size is worth more than the level — the same 50,000 records cost 2,267 ns/record at 16 KiB blocks, 942 at 64 KiB and 556 at 256 KiB, because the per-block work (compress, write, fsync, index row) dominates the per-record work. `Codec::Raw` exists for callers whose bodies are already compressed.

The index is deliberately NOT fsynced per block: it is derived data, adoption rebuilds it from the block headers, and dropping that second sync cut the write path by about a third (1,345 → 882 ns/record at 64 KiB blocks).

The index is `<bucket>/part-<n>.idx`, one row per block, appended when the block is written — the same pure-append discipline as the log itself, no coordination, no seal-time rewrite:

[first_ts u64][first_offset u64][file_offset u32][len_c u32][len_u u32][records u32]

Seek: offset → binary search the parts' first rows, then the rows inside the part → `file_offset` → decompress that block → walk records. The only block missing from the index is the OPEN part's last one, and an open part is read by scanning its tail anyway. Time-seek comes free: the rows are in time order, and each part header already carries its own range.

Why the index is per part and not in `log.meta`, and not per bucket:

- `log.meta` is IDENTITY — epoch, precision, bucket width — read by every process at
open and written once. Appending block rows to it grows identity without bound and turns every 64 KiB into a write to the file everything shares.
- A bucket holds 1..n parts, but the part is the self-contained unit (§6's argument,
the same one that puts the dictionary there): an index that spans parts cannot be dropped with one of them, and retention is `remove_dir_all(<bucket>)` only because nothing else lives beside the parts.

A part's footer (written at seal) carries what a reader cannot derive cheaply: records, last timestamp, block count, and the whole-part digest. Deliberately NOT in there: byte sizes (the file has a length and the index has a row per block), the first timestamp (the header has it), and dictionary entries (the dict file counts itself). A footer that repeats what is already on disk is a second copy that can disagree with the first.

### 9. Statistics and partitioning: where the lakehouse comparison stops

A sealed part knows, for free, everything a reader wants before opening it: when it starts and ends, how many records, how many bytes on disk, how many dictionary entries. All of it is PHYSICAL, all of it is a by-product of writing, and that is exactly the point at which the comparison with a lakehouse has to stop.

- **No field-level statistics.** Min/max/distinct of a field requires the schema,
costs a pass over the data, and answers "which rows", which is a query engine's question. It is what Iceberg manifests and a drainer computing Parquet footer statistics are for. A log that grew field-level stats would be a column store with a worse layout and a hotter write path.
- **Partitioning by a field is a path, not an index.** A log declares one body tag at
creation; its value becomes a directory: `<bucket>/p-<hex(value)>/part-<n>.log`. Hex, so any value a body can hold is a legal directory name, and the `p-` prefix keeps it distinct from a bucket directory (all digits). The writer keeps one open part per partition; the dictionary becomes per (partition, part), which is better locality for exactly the fields worth partitioning on; retention stays per bucket, because a bucket is a time interval and a partition is not.
- **Offsets are per stream, and that is a consequence of the block layout.** A block
header carries its first offset and the reader counts up from there — so a part whose records were interleaved with another partition's could not be read back at all if the counter were global. Offsets therefore belong to (bucket, partition) — or to the log itself when no partition is declared, which is the same rule with one stream. Reads return rows in time order across partitions rather than in directory order.
- **One `LOCK` per log, not per partition** — a deviation from what this ADR first said.
The writer that holds the lock owns every partition under the root; a per-partition lock would only matter if two processes could write different partitions of one log, and nothing asks for that yet. The lock is still what makes "one writer" true between processes, but a pid is a PER-PROCESS identity and cannot see two writers that share one: a second `Log` for the same root inside the process passes the pid check and interleaves its appends into the same part. An in-process registry keyed by the canonical root (so `root`, `root/.` and a symlink are one root) closes that half and refuses with `Error::AlreadyOpen`; the entry leaves on `Drop`, so closing a log and reopening it is unaffected.
- Declared at creation and never changed, for the same reason `bucket` cannot change:
a reader that cannot tell which layout a directory is in cannot read it. A record without the declared tag is refused (`MissingPartition`), never filed somewhere plausible — a layout that lies cannot be read back.

## Consequences

- This crate is not a queue and not a time-series database. It has no ack, no
redelivery and no per-consumer cursor (that is a protocol over offsets, and it can live above this file); it has no aggregation (that is a reduce over records, and it can live above too). It is the append-and-read half, done once.
- Offsets are one writer's u64 counter PER STREAM — a stream being a (bucket,
partition) or the log itself when no partition is declared — continued after a restart from the highest offset any part of that stream reached (§9). Truncation never rewinds it.
- The lake path comes for free: a bucket directory is already a window.
- **ADR-0045's segments do NOT move onto this substrate**, and the earlier version of
this bullet had it backwards. What the two share is a convention — a directory of parts, seal by rename, cut a torn tail — and that is about 150 lines. What they do not share is the contract, and it runs in opposite directions on three points: every record here needs a monotone timestamp (the changelog has no clock at all, and giving it one would let an NTP step turn every subsequent commit's capture into an error), every acknowledged record here is fsynced first (the changelog acks after the commit and repairs holes by rescanning, so adopting this would either add an fsync to the write path or make this crate's guarantee false), and retention here is a clock (there it is a per-consumer watermark — a segment may only be dropped once the lake has taken it). Two implementations of the same convention is the cheaper side of that trade; the changelog's is already tested and already drained. The one thing worth borrowing later is block compression inside a segment, which `changelog.rs` can do on its own without taking on a clock.
- okm core is untouched. This is a sibling crate (`okm-series`), depending on
`okm-wire` for nothing more than the varint and nTLV primitives it already owns; it does not satisfy `VirtualStorage`, does not have keys, faces, reduces or a changelog, and does not pretend to be a table.
- Two open questions, deliberately unanswered here: what a *second* writer
(replication) does to offsets and to the dictionary; and whether a part's dictionary should be shared when the same log is written by several shards. Both change the file layout, so both wait for a requirement that needs them.