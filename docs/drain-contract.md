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
after the projection succeeded, so a failed window leaves no directory, no file and an
unmoved watermark. Columns are:

1. the key fields, in declaration order, **non-nullable** — every row has an
   identity;
2. the payload fields, in **declaration order** (hot and cold interleaved as the
   row was written), **nullable**;
3. three **change columns**, which no declaration has a field for because they are what
   the log said about the row rather than part of it (ADR-0058): `okm_op` (`Utf8` —
   `put`, `put_open`, `del`), `okm_slot` (`UInt16`, nullable — which face a removal
   touched, `0` being the whole row) and `okm_seq` (`UInt32` — the op's position inside
   the window). `okm_seq` may skip numbers: an op folded into another row — the open fields
  of a key this window also put — takes no row of its own.

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
      "rows": 2, "deletes": 0,
      "columns": [["org", "UInt32", false], ["level", "UInt32", true], ...],
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
* **A removal is a change row.** A window holding a `DELETE` op writes it as a row with
  `okm_op = 'del'`, the key filled in and the declared columns NULL (the removal has no
  values to state), `okm_slot` naming the face — `0` is the whole row, another slot is
  one of the document's own parts. Nothing is refused and nothing is invented: the
  reader has to filter `okm_op` and keep the last `okm_seq` per key to get current
  state, which is exactly what a change table asks of its reader.
* **Two kinds of op are counted, not written.** A removal of DERIVED state — an index
  entry, a reduce ledger line — carries no key of the table's shape, and neither its put
  nor its delete becomes a row: the write path rebuilds both from the row. The drain
  reports them (`derived-deletes`) so they are not dropped in silence.
* **The dynamic segment is the window's `variant` column.** The open half of a
  document — the names the declaration does not have — has no column of its own, so
  it travels as one **`variant`** column (the name ADR-0034 gives the typed export's
  column, because it is the same column), `Binary` carrying the Parquet Variant wire
  form (`[metadata len u32][metadata][value]`) with the `ARROW:extension:name =
  parquet.variant` extension, nullable where a row has no segment. That is the
  published encoding, not okm's own: a sink that invented a mapping would split the
  ecosystem. It is one column rather than N, because the set of open names is not
  known at export time and a schema that grows per window is a schema no warehouse
  can hold. A key whose row is NOT in this window — only its open fields were written —
  becomes a `put_open` row: declared columns NULL, `variant` carrying the segment. It is
  a change that touched only the names the declaration does not have, and calling it a
  removal (or dropping it) would both be wrong.
* **Dense vocabulary.** `Vocab` columns carry labels inline. That is byte-wasteful
  for a few distinct labels repeated across millions of rows, and it is deliberate:
  labels are what a warehouse can join and read without okm.

## Consumer obligations

* Treat a window file as immutable once written.
* Advance the watermark only to a window that has been **committed** to the lake —
  the watermark IS the lake's commit, and the file it names is the evidence.
* Never reorder. A sink that needs a different physical layout may rewrite rows
  inside a file it owns; it may not reorder across windows.

## Folding

`okm-parquet condense --out <lake dir>` turns a table's change files into ONE file of current
state and commits the removal of the ones it replaced (ADR-0058 § Amendment):

    _delta_log/<n>.json    {"remove": ...} per replaced file, {"add": ...} for the new one

What a reader sees afterwards is the same rows, one per live key: the change rows are gone,
`okm_op` is `put` (or `put_open`), and `okm_seq` is renumbered `0..n-1` — the sequence means
"position in this file", and there is one file now. History is not lost: it is the earlier
versions of the log, which is where a reader asks for it.

Two things it is NOT:

* **not a deletion vector.** A fold replaces whole files (copy-on-write). A row-level delete
  would have to know which file and which row hold the key, so the incremental form is owed
  rather than pretended.
* **not the log's `compact`.** That verb checkpoints the CHANGELOG's segments into a baseline
  and needs the store; this one reads the exported directory and needs nothing from the
  application.

A fold is INCREMENTAL: it rewrites only the files that share a key with another file, or that
hold a removal. A file whose keys are exclusive to it already states final rows — nothing can
arrive later that changes them, so it is neither rewritten nor removed, and remains in `data/`
as the file it always was. The index this needs is the key columns, read with a projection, so
deciding what to fold never reads a payload. A table with nothing to resolve is refused rather
than rewritten: folding it would churn the log without changing what a reader sees.

The drain can do this on its own: `DrainConfig.fold` (CLI `--fold-after-files N` /
`--fold-after-bytes B`) folds a table after its windows are committed, once its change files
pass the threshold. The fold runs AFTER the commit, never inside a window, so the half-commit
rule that guards a window does not cover it — and a fold that fails fails the run rather than
leaving a table that looks folded and is not.

A folded row is the document's two halves from wherever they were written: the declared
columns from the last `put`, and the open fields MERGED across every window that wrote any
(the variant cells are decoded, unioned field by field with the later window winning, and
re-encoded). A later `put` does not erase the open half of the row before it, and `okm_op`
becomes `put` (or `put_open` when the key never had a full row). `okm_parquet::read::open_fields`
is the same decoder, exposed for anyone reading the export.

A `remove` action drops a file from the TABLE; it does not delete bytes. Replaced files stay
in `data/` for anyone reading an older version, and reclaiming them is a vacuum — a separate
question (how long history is kept) this writer does not answer yet.

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
