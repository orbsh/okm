# ADR-0058: The exported table is a Delta table, and the variant column has one shape

**Status**: Accepted (2026-11-10), amended 2026-11-11 (§ Amendment: the export is a change table and a removal is a row) — implemented in the same round: `okm_parquet::delta`
(a module of the drain crate) replaces core's `bridge::iceberg`, the open-fields column becomes the canonical
`struct<metadata, value>`, and the Iceberg metadata encoder (including a hand-written
Avro writer) is deleted rather than kept.
**Date**: 2026-11-10
**Chinese**: [0058-the-exported-table-is-a-delta-table.zh-CN.md](0058-the-exported-table-is-a-delta-table.zh-CN.md)
**Depends on**: ADR-0034 (a table has a name, an export has the shape of a table),
ADR-0045 (the log: capture, segments, drain), ADR-0056 (the segment is the migration
path)
**Supersedes**: the Iceberg view of ADR-0056 §8 — the exported directory's metadata
format, not the decision that the directory is a table.

## Context

ADR-0056 §8 made one directory per table, and the sink wrote Iceberg v2 metadata over
it: `v1.metadata.json`, a manifest list, an Avro manifest, `version-hint.text`. The
encoder was written by hand to keep core dependency-free (708 lines, of which the Avro
container format is a substantial part), and two things were true of it:

1. **Every window rewrote the whole view.** Snapshot append — one snapshot per file —
   was the deferred writer-side work, so the reader saw one snapshot listing every file
   ever written rather than a history of appends. Appending was O(table), not O(change).
2. **Paths had to be absolute `file://` URIs.** The first design used relative paths
   with `location: "."`, on the theory that a reader resolves against `location`. It
   does not: DuckDB resolves the snapshot's `manifest-list` against the process's
   working directory, so a table opened only when the reader happened to stand inside
   it. Absolute URIs fixed the read and bought a relocation cost — moving the directory
   means rewriting its metadata.

A third fact is not about the format at all: a `variant` column was being written as ONE
flat `Binary` holding `[metadata][value]`, tagged `parquet.variant`. Measured against a
real reader, that column is readable as bytes and decodable by nothing:
`variant_extract(variant, 'note')` answered NULL for every row, while the same metadata
and value bytes in a `struct<metadata: binary, value: binary>` decoded to `first`. The
canonical Arrow extension is `arrow.parquet.variant` over exactly that struct, and both
Delta and Iceberg v3 spell a variant column that way.

## Decision

**The exported directory is a Delta table.** `_delta_log/NNNNNNNNNNNNNNNNNNNN.json`
holds one JSON action per line: the first version declares `protocol` and `metaData`,
and every later version is a commit of `add` actions, one per file the window wrote. The
table is the directory; no catalog, no registration, and no absolute paths (`add.path`
is relative to the table directory by specification).

**It lives in `okm-parquet`, not in core.** The log is a projection of the change log,
one step further out than the window: core produces segments and joins them into a
window, and what a table FORMAT does with that window — Parquet file, log commit, replay
— is the drain crate's work. An encoder in core would make core the owner of a lake's
layout decisions, which is what ADR-0045 §7 and ADR-0056's "only the formats move out"
forbid. (The typed export path of ADR-0034 — `to_record_batch`, the graph's edge table,
`parquet_io` — moved there too: core carries no Arrow or Parquet dependency at all, so
the projection and the type it reads are on opposite sides of the crates, joined by the
`ToRecordBatch` / `EdgeTable` / `EdgeRestore` traits.)

**A variant column is declared `variant`,** with `minReaderVersion 3` /
`minWriterVersion 7` and `readerFeatures/writerFeatures = ["variantType"]`, and is
written physically as `struct<metadata: binary, value: binary>` with the
`arrow.parquet.variant` extension. The feature is table-wide by design: a reader that
does not know it must refuse the table rather than guess what the column means.

## Why Delta and not Iceberg

- **The writer is a log, not an encoder.** Appending a window is one line; there are no
  manifests, manifest lists, snapshots, Avro containers or version hints, and the
  metadata is read back from the Parquet footer rather than restated by the caller. The
  deferred "snapshot append" of ADR-0045 is not built here — it stops being a problem.
- **Paths are relative.** The relocation cost of ADR-0056 §8 disappears, and with it the
  class of bug that cost that ADR a round of repair.
- **Nothing to register.** A reader is pointed at the directory. Iceberg can be read the
  same way, but its metadata is an indirection (directory → metadata.json →
  manifest-list → manifest) that path-based readers have to be told about.
- **No catalog is required for either format at path level** — this was checked rather
  than assumed: `iceberg_scan(<dir>)` and `delta_scan(<dir>)` both work today. The
  catalog argument that motivated this ADR is real only for warehouse engines
  (Trino/Athena/BigQuery), and it is not the deciding one; the writer is.

## What this gives up

- **Iceberg's delete files.** Iceberg v2 positional/equality deletes are mature and read
  by DuckDB, Trino and Spark today; Delta's counterpart is deletion vectors (Spark 3.4+,
  delta-rs and DuckDB partial). Neither is used yet — the sink refuses removals by
  contract — but when the sink learns to express a delete, the weaker side of the two is
  now ours. This is the open item, not a footnote.
- **Warehouse breadth.** Snowflake and BigQuery read Iceberg natively (Delta needs
  external tables / BigLake registration); Athena reads both. For a table that is read by
  DuckDB, Polars or Spark — the readers this path was built for — nothing changes.

## Consequences

- `bridge::iceberg` is deleted, including `stamp_batch`: Delta identifies columns by
  NAME, so a Parquet field id is a second identity nothing reads. `okm_parquet::delta` is
  ~350 lines with no Avro encoder, and `parquet_io` writes the batch unchanged.
- `Report::metadata` counts Delta commits, one per table per window.
- **Unsigned columns widen, visibly.** Delta has no unsigned integer type, so `u8/u16`
  become `short/integer`, `u32` becomes `long`, and `u64` becomes `decimal(20,0)`. A
  reader sees `decimal(20,0)` where okm had a `u64`; the alternative was a column that
  cannot be written at all, or a narrowing cast that loses data.
- **The log carries per-file statistics**: `numRecords`, and `minValues` / `maxValues` /
  `nullCount` for the columns whose Parquet statistics can state the declared type's
  bound (signed integers, floats, strings cut to 32 characters, booleans). Unsigned
  columns carry no bound: their physical statistics are computed on the signed type, and
  a bound derived from them would be wrong for exactly the rows that overflow — an
  invented bound is worse than no skipping.
- The `okm-iceberg` crate named by ADR-0045 and the PLAN is not needed for appending;
  if a writer with partitioning, delete files/vectors and file-size targets is ever
  built, it is still its own crate — that reasoning was never about the format.
- Verified with a real reader, from a working directory outside the table: DuckDB 1.5.5
  `delta_scan(<dir>)` lists the columns including `variant variant`, and
  `variant_extract(variant, 'note')` returns `first` for the row that has an open field
  and NULL for the rows that do not.

## Amendment (2026-11-11): the exported table is a change table, and a removal is a row

The first cut of this export REFUSED any window that carried a removal: a column file has
no place for "this row is gone", and inventing a tombstone column for *some* windows would
give one table two schemas. That was right about the constraint and wrong about the
conclusion — the table is not a current-state table and never was (a window is history,
ADR-0056 §5), so the fix is to say what a row MEANS, table-wide, once:

- `okm_op` (`Utf8`, non-null): `put`, `put_open` (a change that touched only the names the
  declaration does not have — declared columns NULL, `variant` carrying the segment) or
  `del`.
- `okm_slot` (`UInt16`, nullable): for a `del`, the face it removed — `0` is the whole row.
- `okm_seq` (`UInt32`, non-null): the op's position inside the window, so a put followed by
  the purge that removed it stays ordered instead of needing a rule the sink invents.

The order comes from core: `Window` gained `ops: Vec<WindowOp>` (`window.rs`), because the
log's op order is part of what the log said and the join used to keep rows and removals in
two unordered vectors. Two op kinds stay counted rather than written — a removal of derived
state (an index entry, a ledger line) has no key of the table's shape and is rebuilt from
the row, exactly like the put of the same faces.

Three consequences, all deliberate:

1. **Every window is expressible.** A delete no longer blocks the rows beside it, and the
   reader decides what a removal means: filter `okm_op`, keep the last `okm_seq` per key,
   or compact.
2. **Delta's other half is used by REPLACEMENT, not by patching.** `okm-parquet condense`
   folds a table's change files into one current-state file and commits a `remove` per file
   it replaced plus one `add` (copy-on-write). That needs a read path — which landed with it
   (`read::current_state`, identity from the `okm.key` metadata) — but no key→file index,
   because a fold replaces whole files. A ROW-level removal (a deletion vector over an
   untouched file) would need exactly that index, or one row per file; it stays owed, and so
   does partitioning. Folding a document whose halves were written by different windows works
   because a row is assembled column-group by column-group: declared columns come from the last
   `put`, and the open fields are decoded from every `variant` cell the key ever wrote, unioned
   field by field and re-encoded. The fold is incremental
   by key: only files that share a key with another file (or hold a removal) are replaced, so
   a table's untouched history stays where it is. Reclaiming the bytes of replaced files is a
   separate verb with a retention window counted in VERSIONS (`okm-parquet vacuum
   --keep-versions`), because a `remove` action addresses the table, not the file system. A change row still needs neither, and it is what a CDC consumer asks
   for anyway.
3. **The schema is constant per table.** These columns appear on every window of every
   table, carrying `put` / null / 0 where nothing else applies: a schema that shows up only
   on windows that happen to hold a removal is a schema no reader can rely on. Exports
   written before this amendment lack the columns — re-drain from the beginning (a fresh
   out directory, or a consumer name that has no position) instead of mixing two shapes in
   one table.

One latent defect surfaced while this landed, and it was in the statistics, not the
columns: `file_summary` read each field's Parquet statistics BY ARROW INDEX, but a struct
column is one Arrow field and several Parquet leaves, so a field after `variant` was read
from the wrong leaf. With `variant` last nothing noticed; with three columns after it, the
variant's bytes were attributed to `okm_op` and `nullCount` counted the wrong leaf — and a
real reader (DuckDB 1.5.5) then answered ZERO rows for `where okm_op = 'put'` because the
bounds it trusted were nonsense. Resolved by PATH (`leaf_of`), and pinned by
`statistics_name_the_right_column_when_a_struct_sits_before_it`.

Held by `okm-parquet/tests/drain_test.rs`: a window holding a put and the purge that follows
writes two rows (`put` at `okm_seq` 0, `del` at 1 with the declared columns NULL and
`okm_slot` 0), the Delta log gains one `add` and no `remove`/`deletionVector`, and an
open-fields segment with no row becomes `put_open` instead of stopping the drain.
