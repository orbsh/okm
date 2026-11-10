# ADR-0058: The exported table is a Delta table, and the variant column has one shape

**Status**: Accepted (2026-11-10) — implemented in the same round: `bridge::delta`
replaces `bridge::iceberg`, the open-fields column becomes the canonical
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
  NAME, so a Parquet field id is a second identity nothing reads. `bridge::delta` is
  ~350 lines with no Avro encoder, and `parquet_io` writes the batch unchanged.
- `Report::metadata` counts Delta commits, one per table per window.
- The `okm-iceberg` crate named by ADR-0045 and the PLAN is not needed for appending;
  if a writer with partitioning, delete files/vectors and file-size targets is ever
  built, it is still its own crate — that reasoning was never about the format.
- Verified with a real reader, from a working directory outside the table: DuckDB 1.5.5
  `delta_scan(<dir>)` lists the columns including `variant variant`, and
  `variant_extract(variant, 'note')` returns `first` for the row that has an open field
  and NULL for the rows that do not.