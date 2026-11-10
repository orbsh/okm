# The drain contract

What `okm-parquet` promises a sink, and what a sink promises back. The window it
hands over is core's (`okm_core::changelog::window`); this document is the contract
of the projection on top. Written once so
that every adapter (this repo's, and a third party's) speaks the same dialect
instead of inventing one per warehouse. ADR-0045 is the decision; this file is the
interface that follows from it.

## The artifact

One **window** is one Parquet file inside the table's exported directory:

    <out>/<table hex>/data/<table hex>-<first segment>-<last segment>.parquet

The directory per table is the exported TABLE (ADR-0056 §8): `data/` holds the
windows, `_delta_log/` is the Delta log over them, and the consumer's watermark sits
at the root (`watermark-<consumer>-<table hex>.txt`). Two tables in one sink
are two tables, not one table with `0047-` and `0048-` files in it.

The file is written under a scratch name at the root and moved into `data/` only
after the projection succeeded, so a refused window (a delete has no column) leaves
no directory, no file and an unmoved watermark. Columns are:

1. the key fields, in declaration order, **non-nullable** — every row has an
   identity;
2. the payload fields, in **declaration order** (hot and cold interleaved as the
   row was written), **nullable**.

Arrow types come from `okm_core::arrow_backend::arrow_type` and nowhere else:

| declared kind | Arrow type |
| --- | --- |
| `u8` / `u16` / `u32` / `u64` | `UInt8` / `UInt16` / `UInt32` / `UInt64` |
| `VarInt<T>` | `UInt64` (the integer; the varint is a frame, not a value) |
| `Quant<P>` | `Float64` (fixed point already divided by `10^P`) |
| `Offset<B>` | `Int64` (already rebased by `B`) |
| `String` | `Utf8` |
| `[u8; N]` / `Vec<u8>` / `Vector<T>` | `Binary` (a Vector as its frame) |
| `Vocab<N>` | `Utf8` — the **label**, never the id |

Columns are identified by NAME, in the log and in the files: the metadata records a
Delta schema, which has no field ids, so a Parquet field id would be a second identity
nothing reads.

## The manifest

Beside every window file:

    <table hex>-<first>-<last>.manifest.json

    { "table": "<header hex>", "layout_version": 3, "segments": ["00000001.segment", ...],
      "rows": 2, "columns": [["org", "UInt32", false], ["level", "UInt32", true], ...],
      "file": "<table hex>-<first>-<last>.parquet" }

`columns` is in column order (keys first) with the Arrow type name and the
nullability the file declares. The manifest is written BEFORE the watermark moves,
so a consumer that sees a watermark can read a description of what it points at —
and a window is never half-described.

## What a window means

* **Append-only.** A window is history, not a table: the same key may appear twice
  with different values, and rows are in log order (commit order). A window does
  not repeat the order promise ACROSS windows — that is the store's, and no sink
  may weaken it to buy a cheaper capture.
* **Current state is the sink's job.** A sink routes a window to a current-state
  view one of three ways: a native upsert key on the key columns, delete files, or
  periodic compaction that rewrites the table. okm does not choose for it.
* **Deletes are refused, not encoded.** The drainer stops on a window holding a
  `DELETE` op: the declaration has no column for a removal, and a tombstone column
  nobody else reads would change every other reader's view. Until compaction lands,
  a sink that cannot express deletes is safe by construction. The same stop covers a
  window that holds an `open` segment whose row is absent — a removal seen from the
  segment's side.
* **The dynamic segment is the window's `variant` column.** The open half of a
  document — the names the declaration does not have — has no column of its own, so
  it travels as one **`variant`** column (the name ADR-0034 gives the typed export's
  column, because it is the same column), `Binary` carrying the Parquet Variant wire
  form (`[metadata len u32][metadata][value]`) with the `ARROW:extension:name =
  parquet.variant` extension, nullable where a row has no segment. That is the
  published encoding, not okm's own: a sink that invented a mapping would split the
  ecosystem. It is one column rather than N, because the set of open names is not
  known at export time and a schema that grows per window is a schema no warehouse
  can hold. A key whose row is gone carries no row here — and a window that holds
  such a segment is REFUSED, not written: the row is a removal by another route, the
  segment has nothing to attach to, and dropping it in silence is exactly what this
  contract exists to prevent.
* **Dense vocabulary.** `Vocab` columns carry labels inline. That is byte-wasteful
  for a few distinct labels repeated across millions of rows, and it is deliberate:
  labels are what a warehouse can join and read without okm.

## Consumer obligations

* Treat a window file as immutable once written.
* Advance the watermark only to a window that has been **committed** to the lake —
  the watermark IS the lake's commit, and the file it names is the evidence.
* Never reorder. A sink that needs a different physical layout may rewrite rows
  inside a file it owns; it may not reorder across windows.

## Not part of the contract

Partitioning, target file size, catalog registration, snapshot management and
retention all belong to the adapter. `okm-parquet`'s only knob is
`DrainConfig::max_segments` — how much history one window carries — because that is
also how much history a query must merge with the hot store.

## The table

After the watermark moves, the directory is a Delta table: `_delta_log/00000000…json`
holds the first commit (protocol + schema + one `add`), and every later window is one
more version holding one `add`. Appending is one line, so the log grows by the change
and not by the table — this is the "snapshot append" a manifest-based writer has to
build, and Delta gets from the format.

Columns and row counts are read FROM the written files (the Parquet footer), never
restated by the caller. A path in a commit is relative to the table directory, so the
directory travels intact: no absolute URIs, no relocation cost, no version hint to
keep in step.

The open-fields column is declared `variant`, which is a table-wide promise: the
protocol becomes `minReaderVersion 3` / `minWriterVersion 7` with
`readerFeatures/writerFeatures = ["variantType"]`, and a reader that does not know the
feature must refuse the table rather than guess. That is the price of a column engines
actually DECODE — the same values as a plain binary are readable everywhere and
understood nowhere (measured: `variant_extract` answered NULL for every row of the
flat form, and decoded correctly from the canonical one). The physical shape is
`struct<metadata: binary, value: binary>` with the `arrow.parquet.variant` extension,
which is what both Delta and Iceberg v3 specify.

## Not part of the contract

Partitioning, target file size, catalog registration, snapshot management and
retention all belong to the adapter. `okm-parquet`'s only knob is
`DrainConfig::max_segments` — how much history one window carries — because that is
also how much history a query must merge with the hot store.

## The Iceberg view

After the watermark moves, `metadata/` holds what a catalog needs: the manifest, the
manifest list, `v1.metadata.json` and `version-hint.text` (DuckDB finds the current
metadata through the hint; pyiceberg and a catalog take the metadata path). Columns
and row counts are read FROM the written files, never restated by the caller, and
field ids are stamped in the Parquet in column order, so nothing needs a name
mapping.

Every recorded path is an absolute `file://` URI, including the table's `location`.
That is deliberate and it was measured: relative paths were the first design (a
bundle that travels when moved), but a real reader does not resolve them against
`location` — DuckDB's `iceberg_scan` resolves the snapshot's `manifest-list` against
the process's working directory, so a table recorded as `location: "."` opened only
when the reader happened to sit in the table directory. An absolute URI needs no
base. The price: relocating the directory means rewriting the metadata, which is the
deal every Iceberg writer makes.

The metadata is rebuilt over the whole `data/` directory on every window, because
the writer side — snapshot append (one snapshot per file), delete files,
partitioning, file-size targets — is not this crate's yet. Until it is, a reader sees
one snapshot listing every file written so far, not a history of appends.
