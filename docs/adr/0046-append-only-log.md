# 0046 — The append-only log: one bucket per directory, deltas for time, and a dictionary that earns its place

> **Languages:** [English](0046-append-only-log.md) (primary) · [中文](0046-append-only-log.zh-CN.md)

**Status:** Proposed (2026-10-28) — design only; nothing is implemented.
**Date:** 2026-10-28

## Context

The need is a log: append a record in time order, read it back by offset or by
time, and drop old data without paying per-record deletion. Queues are a layer
above it, time series are a use of it. The pressure that makes it its own thing
rather than an okm table is retention: a consumed message and an expired sample
are both "gone", and in an LSM every one of them leaves a tombstone behind, so the
engine spends its compaction budget on a table whose only visible effect is
shrinkage (aura's `MqData` is exactly this shape: append with a `HighWater` seq,
delete below the slowest cursor).

Four facts decide the shape.

- **Wall-clock time has to survive with a declared precision.** A queue needs
  order, a series needs the instant. Storing a `u64` nanosecond stamp per record
  costs 8 bytes on every line for information that is almost always a small step
  from the previous one.
- **Retention is by time, so the FILE must be the retention unit.** Anything that
  drops "old rows" row by row reproduces the problem the log exists to avoid.
- **The payload is nTLV, not opaque bytes.** A generic broker stores bytes and
  lets every consumer invent a schema; that freedom is worth nothing in 99% of
  streams and costs them a decoder each. nTLV is a small self-describing
  encoding, and converting it to anything else is a walk over items.
- **A string dictionary is a bad default and a good exception.** For a stream of
  events whose field names and values are one of twenty shapes, interning is a
  large win; for a stream whose keys are unique per record (ids, request paths,
  user agents), a dictionary is a pure loss that grows without bound. The encoding
  has to be able to go both ways **in the same file, decided per key by
  evidence**.

## Decision

### 1. The bucket: hours on the wall clock, a directory each

A log is a directory. Its `log.meta` declares three things that never change:
`epoch` (Unix ms at creation, for humans and for negative-time refusal),
`precision` (s / ms / µs / ns — the quantization unit the writer applies), and
`bucket` (seconds per bucket, default 3600).

Bucket index = `floor(unix_seconds / bucket)`. Absolute alignment, not
creation-relative: an hour file is the same interval for every log on the machine,
which is what makes several logs joinable by name and what makes a shard's
directory listing meaningful.

    <log>/log.meta
    <log>/LOCK                      single writer, held for the process lifetime
    <log>/<bucket>/part-<n>.log     records, append-only, sealed by rename
    <log>/<bucket>/part-<n>.dict    the strings that earned a code, same lifetime

A part is sealed by renaming `.partial` → the final name (ADR-0045's discipline,
reused). A part may rotate early on a size cap; then a bucket holds several parts,
and retention is `remove_dir_all(<bucket>)` — one syscall per hour, no tombstones,
no scan.

### 2. Time inside a part: a base plus deltas

The part header carries `first_offset` (u64), `base_ts` (full timestamp of the
first record in the declared precision), `bucket` (u32, repeated so a file is
self-describing), and `record_count` (filled at seal time; 0 while open).

A record is:

    [len u32][ts Δ varint][nTLV body]

`ts = previous_ts + Δ`, the first record's Δ being `base_ts`. Deltas are small for
a busy stream (one or two bytes at any precision) and honest for a sparse one (a
varint grows instead of the file lying about resolution).

**Timestamps must not go backwards.** A record older than its predecessor is
rejected by the writer, not clamped: the file's byte order IS the time order, and a
file whose order and clock disagree cannot be read by either. A caller with skewed
writers gets one writer per log (the `LOCK` file) and an error it can act on.

**There is no per-record checksum, and that is a decision.** Given §3 (nothing is
acknowledged before it is fsynced) and the fact that a part is only ever written at
its end, the only damage a crash can do is truncate the tail — so the torn record is
the one whose `len` runs past the end of the file, and that one rule is the whole
recovery procedure. A checksum would not catch more of a crash; it would catch a
storage stack that LIED (media bit rot, a network filesystem, an fsync that did not
happen), and integrity against that belongs to whoever owns the bytes: the device's
ECC, the filesystem's checksums, the object store's own validation. A record format
cannot repair a flipped byte anyway — it can only refuse to read it, which turns a
data problem into an outage. What the substrate does own is the BOUNDARY: a sealed
part carries a whole-part digest in its footer (§8), so a copy, an export or a
transport verifies what it carries with one hash instead of one per record.

Reading a part is sequential: "read from offset N" = pick the part, then seek inside
it through its block index (§8).

### 3. Durability inverted

ADR-0045 appends AFTER the commit, at most once, and a hole is repaired by
rescanning the store. This log has no store behind it: `append` returns an offset
only after the bytes are fsynced. The costs differ (one fsync per append unless the
caller batches; a `sync` verb for callers that accept a window), the discipline does
not: an acknowledged record is on disk.

### 4. nTLV, and where strings go

The item number carries the string's FORM, which is what makes escalation a
one-byte change and keeps the hot path branchless:

- `n = 0` — inline: `[len varint][utf8 bytes]`.
- `n = 1` — dictionary #0, body is that dictionary's width (u16).
- `n = 2` — dictionary #1, width u32.
- `n = 3` — dictionary #2, width u64.

Dictionary #k's width is declared in its block header (`u16 << k`), so the body
length is derivable and the reader never guesses. The same encoding applies to the
tag half and the value half of an item: an nTLV document's *field names* are
strings too, and a schemaless stream is exactly the case where they repeat.

### 5. When a string earns a code: tiers of its own width

A dictionary is per part (§6). While a part is open the writer counts
`string → (count, id_or_none)` and promotes a string when its count reaches the tier
its LENGTH falls in. Bytes are the unit: `L` is the string's byte length, `W` the
width of the block it would land in (2, 4 or 8).

| length | promote at | why |
| --- | --- | --- |
| `L <= W` | never | a string no longer than its own code can only grow: one byte of `GET` against a u16 code is a loss at every count |
| `W < L <= 4W` | 10 | the short end: a code saves little per hit |
| `4W < L <= 8W` | 5 | |
| `8W < L <= 16W` | 3 | |
| `16W < L` | 2 | the long end: one more repetition already pays |

The last row is why a long error-log message interning at its second occurrence is
not a special case — it is the tier doing its job, and it is also the row that keeps
"a fixed 10" from being the whole policy.

The table is configuration, not format: `[[4, 10], [8, 5], [16, 3], [0, 2]]`
(multiple of the code width, promotion count; `0` = no upper bound). Changing it
changes what FUTURE parts promote and no reader ever needs to know, which is the
property a tunable has to have.

Two things the tiers do not express, and both matter:

- **The gate is absolute.** Shorter than its own code is never worth interning, at
  any count, in any configuration. This is the one rule that cannot be tuned.
- **Counts are per part, so they restart every hour.** A threshold is therefore paid
  again each hour: a message that appears nine times an hour is promoted by the
  `16W` tier and never by a global constant of 10. This is the strongest argument for
  the short end being the only conservative one.

**Why not the exact cross-over.** With `L_+ = L + length field` the arithmetic
break-even is `f* = ⌊L_+ / (L_+ − W)⌋ + 1` — for an 8-byte string against a u16 code,
`f* = 2`, so the tiers are 5x conservative there and exact at the long end. The trade
is deliberate: being early saves `(f − f*) × (L_+ − W)` per later occurrence, being
wrong costs one copy of the string inside one hour, and a four-row table is something
an operator can reason about while a division with two derived quantities is not.
If a part's dictionary turns out to pay at the short end, tighten the list — the knob
exists for that, and nothing on disk changes.

Nothing is rewritten when a string is promoted late: earlier records keep their
inline copies, which is why a part's dictionary only ever grows and the file stays
append-only. A stream of unique keys promotes nothing and stores everything inline:
unbounded cardinality is the default path here, not a failure mode.

The counter is not persisted. A part's counts measure a window that dies with the
part; a persisted counter would be a second source of truth for a decision whose
worst case is one wasted copy.

### 6. Why the dictionary belongs to the part, not the log

A log-global dictionary is the tempting design (one table, extension blocks chained
from the header) and it is the wrong one here:

- A part must be **self-contained** — it is the retention unit, the export unit,
  and the unit a replication or lake consumer copies. A part that cannot be decoded
  without a file that outlives it cannot be dropped, archived or shipped alone.
- A log-global dictionary can never be pruned while any part references an entry,
  so it needs refcounting or a GC pass — machinery whose failure mode is silent
  corruption.
- Frequency is local in time. An hour is the right window for "is this string hot",
  and a per-part dictionary measures exactly that window.

The cost is duplication: a string repeated every hour is stored once per hour. That
is bounded, predictable, and paid only for strings that recur anyway.

### 7. The escalation chain, kept inside the part

Dictionary #0 sits at a fixed offset in `part-<n>.dict` (straight after its header)
so the common case needs no indirection. Each block header holds `width`, `count` and
`prev_offset`; when a block's id space is exhausted (65,535 at u16) a new block is
appended with double the width and its `prev_offset` names the block before it.

The chain is walked BACKWARDS from the newest block, and here is why that matters:
the newest block is named by `<bucket>/part-<n>.dict.tail`, a four-byte file replaced
by rename. Rename is atomic, so growing the dictionary — the only event that used to
require patching a header in place — becomes another append plus a rename. **A part
therefore has no in-place mutation at all**, which is what makes §2's claim true: the
only thing a crash can damage is the tail of a file, and the only tail that matters
is the one being appended to. A reader that meets `n = 3` before any `n = 2` walks
`prev_offset` twice. The chain is per part, so "widen" happens at most twice in the
life of a file.

### 8. Blocks: compression, and the index that makes a part seekable

A part's body is a sequence of BLOCKS: 64 KiB of records by default, each compressed
on its own (zstd level 1–3; lz4 where the CPU matters more than the ratio).

- **Blocks, not records.** A 40-byte record compressed alone is header plus noise —
  a zstd frame costs ~13 bytes before it saves anything. The repetition worth
  compressing lives BETWEEN records (the same field names, the same level values,
  the same URL shape), and only a block can see it.
- **Every block, not only the long ones.** A size threshold makes two shapes of one
  file and still cannot know whether a record would have compressed: the ratio is a
  property of its neighbours, not of its own length.
- **At flush, not at seal.** Compression happens when the block buffer fills, so the
  append path pays it once per 64 KiB instead of once per record, and the file is
  written once. Compressing at seal would give the codec the whole hour and would
  turn every seal into a rewrite of the part — 2x write amplification for a few
  percent of ratio.

**This partly overlaps the dictionary (§5), and the ADR says so instead of pretending
otherwise.** A block codec with back-references already collapses a repeated string to
about the size of a code; interning removes the repetition before the codec sees it,
which makes the codec faster and the stream smaller, but its marginal byte win is
smaller than it looks in isolation. `dict_policy` is therefore a per-log switch, and
the choice belongs in a measurement, not in an argument.

The index is `<bucket>/part-<n>.idx`, one row per block, appended when the block is
written — the same pure-append discipline as the log itself, no coordination, no
seal-time rewrite:

    [first_ts u64][first_offset u64][file_offset u32][len_c u32][len_u u32][records u32]

Seek: offset → binary search the parts' first rows, then the rows inside the part →
`file_offset` → decompress that block → walk records. The only block missing from the
index is the OPEN part's last one, and an open part is read by scanning its tail
anyway. Time-seek comes free: the rows are in time order, and each part header already
carries its own range.

Why the index is per part and not in `log.meta`, and not per bucket:

- `log.meta` is IDENTITY — epoch, precision, bucket width — read by every process at
  open and written once. Appending block rows to it grows identity without bound and
  turns every 64 KiB into a write to the file everything shares.
- A bucket holds 1..n parts, but the part is the self-contained unit (§6's argument,
  the same one that puts the dictionary there): an index that spans parts cannot be
  dropped with one of them, and retention is `remove_dir_all(<bucket>)` only because
  nothing else lives beside the parts.

A part's footer (written at seal) carries what a reader cannot derive cheaply:
records, last timestamp, block count, and the whole-part digest. Deliberately NOT in
there: byte sizes (the file has a length and the index has a row per block), the first
timestamp (the header has it), and dictionary entries (the dict file counts itself).
A footer that repeats what is already on disk is a second copy that can disagree with
the first.

### 9. Statistics and partitioning: where the lakehouse comparison stops

A sealed part knows, for free, everything a reader wants before opening it: when it
starts and ends, how many records, how many bytes on disk, how many dictionary
entries. All of it is PHYSICAL, all of it is a by-product of writing, and that is
exactly the point at which the comparison with a lakehouse has to stop.

- **No field-level statistics.** Min/max/distinct of a field requires the schema,
  costs a pass over the data, and answers "which rows", which is a query engine's
  question. It is what Iceberg manifests and a drainer computing Parquet footer
  statistics are for. A log that grew field-level stats would be a column store with
  a worse layout and a hotter write path.
- **Partitioning by a field is a path, not an index.** A log may declare one
  partition field at creation: `<bucket>/<partition value>/part-<n>.log`. The reader
  walks the partition directories it needs; the writer keeps one open part per
  partition, and therefore one `LOCK` per partition (the single-writer rule is per
  file, and a partition is a file's worth of writer). The dictionary becomes per
  (partition, part), which is better locality for exactly the fields worth
  partitioning on. Retention stays per bucket — a bucket is a time interval, not a
  value.
- Declared at creation and never changed, for the same reason `bucket` cannot change:
  a reader that cannot tell which layout a directory is in cannot read it.

## Consequences

- This crate is not a queue and not a time-series database. It has no ack, no
  redelivery and no per-consumer cursor (that is a protocol over offsets, and it can
  live above this file); it has no aggregation (that is a reduce over records, and it
  can live above too). It is the append-and-read half, done once.
- Offsets are a single writer's u64 counter, continued across restarts from the last
  sealed part's header. Truncation never rewinds it.
- The lake path comes for free: a bucket directory is already a window.
- **ADR-0045's changelog segments become this substrate.** It is the first
  append-only file format in the repo and this is the second, which is one too many:
  the envelope, the seal/rotate discipline, retention and torn-tail recovery move
  here, while the changelog keeps its body codec (key-only `OpFrame`s) and its
  durability policy (append AFTER the commit, at most once — the opposite of §3).
  That split is the point of a substrate: what differs between the two is exactly the
  two plug-ins, and what does not is a file format nobody should write twice.
- okm core is untouched. This is a sibling crate (`okm-series`), depending on
  `okm-wire` for nothing more than the varint and nTLV primitives it already owns;
  it does not satisfy `VirtualStorage`, does not have keys, faces, reduces or a
  changelog, and does not pretend to be a table.
- Two open questions, deliberately unanswered here: what a *second* writer
  (replication) does to offsets and to the dictionary; and whether a part's
  dictionary should be shared when the same log is written by several shards. Both
  change the file layout, so both wait for a requirement that needs them.