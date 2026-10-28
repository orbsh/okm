# 0045 — The change log: capture at the commit boundary, drain out of process

> **Languages:** [English](0045-changelog-capture-and-drain.md) (primary) · [中文](0045-changelog-capture-and-drain.zh-CN.md)

**Status:** Accepted (2026-10-28) — design only; nothing is implemented.
**Date:** 2026-10-28

## Context

The proposal was to record every write at the storage layer, accumulate by time
and count the way a rolling log does, and turn the accumulation into Iceberg
files once it is big enough. Counting and rotation are the easy half; six facts
about this codebase decide what the record can be and where it may live.

- **The storage boundary is bytes and nothing else.** `VirtualStorage` speaks
  `put`/`get`/`del`/`scan`/`commit_batch`; slot meaning lives in
  `model/index.rs`, and reading a payload back needs the declaration. A capturer
  at that boundary sees physical ops, not rows.
- **One semantic write is one `Batch`, and the batch is more than rows**: the
  primary entry, one entry per declared face, embedded children, the reduce
  fold/unfold, vocabulary growth. Index, reduce and junction entries are derived
  state, and the export contract already excludes them (PLAN Phase 4).
- **The subscribe sink cannot carry it.** Emission is best-effort by contract,
  and `save_with_prev_into` deliberately emits nothing (ADR-0031 §5) — so
  `merge`, `replace`, `purge` and every cross-collection batch write would be
  invisible.
- **`epoch` is in-process.** A per-collection `Cell` seeded at 0 in `new`,
  bumped after each commit, reset on restart (ADR-0008 §5). It orders writes
  inside one process lifetime and cannot address anything durable.
- **A key's time is the application's declaration.** okm keeps no implicit
  timestamp anywhere; keys are fixed-width and `KeyEncode` rejects `String`. So
  "everything newer than T" is not a question every table can answer, and a
  scan-based delta only exists for tables that lay their key out to allow it.
- **The engines are the wrong shape for a queue.** fjall/slatedb/redb are LSM or
  B-tree stores: appending and then deleting in key order maximizes compaction
  churn, every record carries per-entry overhead it does not need, and
  truncation costs one delete per drained entry.

## Decision

**1. The capture point is `Collection::commit`,** the private funnel every
semantic write passes through — `put`, `delete`, `merge`, `replace`, `purge`, and
the callers of `save_with_prev_into`. Filtering happens in memory before the
commit; the append happens only after `commit_batch` returns `Ok`. Not the
storage boundary (it cannot tell a row from an index entry), not the subscribe
sink (ADR-0031 §5).

**2. One committed batch is one frame, encoded by `okm_wire::OpFrame`.** That is
the encoding the remote path already uses (`write_batch` over
`(key, Option<value>)` ops, keys and values opaque). A file is a byte stream, so
the writer adds the one thing the codec deliberately does not carry: an outer
length prefix. No second format, and vocabulary growth needs no record of its own
— it is already an op in the batch.

**3. The filter is an EXCLUDE list, stated once, and it fails closed toward the
lake.** Carried — the set an export carries: primary `0x00`, dynamic segment
`0x01`, field-name vocabulary `0x02`/`0x03`, predicate vocabulary `0x04`/`0x05`.
Dropped: declared index `0x3nnn`, reduce `0x4nnn`, junction `0x5nnn`, graph faces
`0x06`–`0x09` (`Graph::reindex` empties and rebuilds them from the primary rows,
so an edge rides as a row like anything else), vector snapshot `0x0A`, table meta
`0x0F`, and **bare-face keys** — a key whose length is
the header plus the slot width is a face-level singleton (the vocabulary
allocation cursor, the table's meta row), and no row ever has that shape.
Anything unrecognised is dropped too: a log that is visibly incomplete can be
repaired by a reconciliation scan, a lake that quietly holds derived state cannot.
Faces are allocated in code (ADR-0030/0033), so adding one is a code change, and
this list is a required part of that change — nothing checks it for you.

**4. The log is files, not a face and not a second engine.**
`<root>/<header hex>/<nnnnnnnn>.partial` while it is being written, renamed to
`.segment` on seal — the extension names the artifact, not the transport (`.log`
reads as an application log, which this is not). Rotation on
bytes and on wall clock. Written here and not in the keyspace because the three
costs invert: a plain append is 1x bytes with no per-entry overhead, reclamation
is `unlink` of a whole segment instead of one delete per drained record, and the
hot keyspace's compaction budget is untouched.

**5. The address is `(segment, offset)`, so there is no producer counter.** The
next segment number is the highest name in the directory plus one; the offset is
the append position the writer already holds. No reservation, no hi-lo blocks, no
sequence holes to reason about, and a sealed segment's name is the durable fact
that it is complete.

**6. Order is commit, then append. The guarantee is at-most-once, and never a
phantom row.** A crash between the two loses the tail; the lake is then missing
rows the store still has, which a reconciliation scan repairs. The other order
would put rows into the lake that the store never accepted — a lake that
disagrees with the store is worse than a lake that lags behind it.
`sync_data = true` narrows the hole to one append and never changes the failure
class.

**7. Nothing in core owns the schedule, and the write side has no reader.**
No timer, no executor, no background
thread — the same rule the event layer already follows (`subscribe.rs`: the
sink is whatever the caller registers). okm provides the writer, `seal`, and the
segment reader. A collection with the changelog on appends, rotates, and
otherwise forgets the file; the drain, the export window, the reconciliation scan
and the Iceberg write belong to the process that owns them.

The reading side never opens the store — it opens a directory. So it can be
another process on another machine, fed by whatever moves files (a copy, an
object-store sync, a queue of blobs): a sealed segment is immutable, and the only
two invariants across the boundary are "a `.segment` never changes" and "nothing
is deleted before its consumer's watermark passes it".

**8. Watermarks live where the consumer's commit lives.** One consumer: the act of
committing window W to the lake IS the watermark — a second file kept in step
with it would be a second truth. Several consumers: one small file each,
replaced by atomic rename, never edited in place.

**9. Self-description stops at names.** Names are recoverable because the
vocabulary faces ride along. Types and key layout are not in any byte of the log:
the key is a fixed-width concatenation with no field boundaries, and the payload
is positional. A schema sidecar (`CollectionSchema::of::<K, R>()`, `schema-serde`)
written once per layout version into the same directory is what turns the log
into columns; without it the log is readable as bytes and useless as a table.
The schema is sufficient on its own: it carries `key_len` and `key_fields` with
their offsets and widths, the hot/cold field lists with tags, `layout_version` and
the slot map — the same facts the typed Arrow bridge builds its columns from.

**10. This is not a third event mechanism.** The event layer is `reduce` and
`subscribe` (ADR-0008). This is an entry in the write set plus a file: no
callback, no declaration surface, no delivery promise beyond the ordering in §6.

**11. Opt-in at the assembly point, not in the declaration.**
`Collection::with_changelog(store, config)` is the fallible constructor;
`Collection::new` stays I/O-free (ADR-0043). A derive attribute would make a
per-instance policy a breaking declaration change, and the switch is the same
kind of decision as engine choice. No cargo feature: the writer needs no
dependency, and the cost when off is one `Option` check per commit.

**12. Default is key-only. A payload copy is a different feature.** The record
carries `(op, key)`; a reader that needs the row reads the primary entry, and a
key that is gone means the row is gone. Storing the payload too buys history
(every version, not just the last write) and pays about the row's own bytes on
every write; it needs its own decision, not a flag.

**13. okm produces a window, not a table format.** The output side of this
mechanism is a sealed set of Parquet files beside the schema sidecar plus a
manifest of what the window holds. The three layers are distinct: the store
produces segments and the sidecar, the drainer groups N sealed segments into one
window (the window's size is the DRAINER's policy, because the sink decides its
own file target), and the adapter loads windows into a table format. Iceberg,
Delta and whatever replaces them are
adapters ABOVE that, in their own crate and their own process: each ingests
"Parquet plus a manifest", so the adapter owns the catalog, the snapshot log and
the delete mechanism, and a deployment that wants no lake pays for no lake.
What must not vary with the format is the semantics — a window is an append of
rows as they were committed, a current-state view needs compaction (rewrite) or a
sink that can express deletes, and no sink may buy itself a cheaper capture by
weakening the ordering in §6. Otherwise "format-agnostic" turns into one crate
per format with different guarantees.

An adapter needs three things from a sink and no more: it can hold the key
columns, it can reach "current state" somehow (a native upsert key, delete files,
or periodic compaction), and it can hold the dynamic segment — natively as a
variant type, or degraded to a JSON string under a mapping okm writes down ONCE
(Binary as base64, `Int`/`UInt` kept as numbers, lists and maps as arrays and
objects). Which of the three a sink offers is the adapter's business; the
mapping is not, or every third-party adapter invents its own dialect.

## Rejected

**A face in the same namespace** (`0x0B`–`0x0E` is the free band). The cheapest
to wire — capture is already a batch write, so it commits atomically with the row
— and the most expensive to run: the record pays LSM per-entry overhead, is
rewritten as it moves levels, needs a tombstone, and is compacted away, so a
useful 30-byte record can cost several times that in write amplification; and
truncation is one delete per drained record, which doubles a stream that exists to
save writes. Capture and store are separate decisions, and tying them to get
atomicity buys the wrong half.

**A second local engine instance.** It moves the churn out of the hot keyspace and
stops there: it is still LSM (same per-entry overhead, same compaction), it adds a
second WAL and therefore a second durability domain — a commit that lands in one
and not the other is either a hole or a phantom — and `VirtualStorage` has no
log-structured implementation to point it at.

**The log as the system of record** (append and `fsync` before the commit). It
makes the file authoritative, and then okm owns recovery: an undo/replay of
un-drained records on open. That is a second WAL in front of engines that already
keep one, and it would be the real implementation of `commit_batch`'s durability
rather than a consumer of it. The log answers "what changed"; it must not answer
"what is true".

**Capturing from the subscribe sink.** Zero new write-path code, and silently
wrong: `save_with_prev_into` emits nothing by design (ADR-0031 §5), so
`merge`/`replace`/`purge` and every cross-collection batch would never appear in
the log.

**Capturing index, reduce and junction entries.** They are derived state; the
export contract already leaves them out, and a replay would double-write work the
store rebuilds deterministically.

**A producer sequence counter.** A durable counter is what a keyspace-based log
needs and what a file-based log does not: the segment number plus the offset is
already a total order, and it is free.

**Exactly-once.** A retried write is a second record. Deduping by key inside a
window is the consumer's business, and saying so is cheaper than a uniqueness
guarantee the writer cannot provide.

## Consequences

- Hot path: one `Option` check and one `memcpy` into a buffered writer; no extra
  `fsync`, no extra entry in the hot keyspace. The numbers to compare against are
  the recorded ones: 1.31 µs for a semantic put (in-memory engine), 0.71 µs for a
  256 B fjall put.
- A directory per table header, so a partitioned table's streams cannot collide,
  and the sidecar sits beside the segments.
- Deleting rows from the store after an export is only safe once the window is
  sealed and taken; after a crash the reconciliation scan runs before any
  deletion resumes. This is part of the offload procedure, not the caller's
  discretion — otherwise the at-most-once hole becomes permanent data loss.
- The Iceberg side still lacks what this ADR does not provide: snapshot append
  with delete files, partitioning, and file-size targets. `write_table_metadata`
  writes one snapshot over the files it is handed, so a naive "one flush, one
  table" turns the lake into a set of small unpartitioned tables.
- The drainer needs less than the application and more than the file: `okm-wire`
  reads the frames, and the sidecar supplies the types, so `okm-dynamic` can
  decode payloads against a schema but has no Arrow/`RecordBatch` surface today.
  A drainer that does not link the
  application's rows has to gain one (schema-driven column construction) or link
  the typed assembly point and carry the declarations. Whichever it is, it is
  drain-side work, not a change to the log — but the column builder itself belongs
  in core beside the typed bridge (it must reuse `arrow_type`, or the same
  `FieldType → DataType` mapping exists twice), while the Iceberg writer
  (snapshot append, delete files, catalog) belongs in its own crate: that is the
  piece with real dependencies, and `bridge::iceberg` today is a metadata encoder
  with none.
- The log does not make every table's delta cheap: it is the answer for tables
  with no usable time-ordered dimension. Where one exists, an interval scan over
  the key or an index is the cheaper delta, and this feature earns nothing.
- Configuration:
  `ChangelogConfig { dir, segment_bytes: 64 MiB, segment_seconds: 300, buffer_bytes: 1 MiB, sync_data: false }`
  — the first three are the writer's rotation policy, the last is the only knob
  that changes the size of the crash window.

## Related records

ADR-0008 (the event layer is `reduce` and `subscribe`), ADR-0010 (the storage
boundary speaks bytes), ADR-0030/0033 (face numbering and slot order), ADR-0031 §5
(batch writers emit no events), ADR-0032 (the vocabulary cursor is a bare
prefix), ADR-0034 (table names and the export shape), ADR-0040 (bytes this SDK
cannot decode are refused, never guessed), ADR-0043 (`new` stays I/O-free).