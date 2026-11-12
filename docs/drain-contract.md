# The drain contract

What `okm-lake` promises a sink, and what a sink promises back (the binary that
serves it is `okm-lake`; the log's own side — status, compaction, replay — is `okm-log`). The window it
hands over is core's (`okm_core::changelog::window`); this document is the contract
of the projection on top. Written once so
that every adapter (this repo's, and a third party's) speaks the same dialect
instead of inventing one per warehouse. ADR-0045 is the decision; this file is the
interface that follows from it.

## The artifact

One **window** is one Parquet file inside the table's exported directory:

    <out>/<table hex>/data/[<column>=<value>/]<table hex>-<first segment>-<last segment>[-p<k>].parquet

The directory per table is the exported TABLE (ADR-0056 §8): `data/` holds the
windows, `_delta_log/` is the Delta log over them, and the consumer's watermark sits
at the root (`watermark-<consumer>-<table hex>.txt`). Two tables in one sink
are two tables, not one table with `0047-` and `0048-` files in it.

A partition directory appears when the sink is partitioned by a key column, and the `-p<k>`
suffix when a window is cut by the row limit (both below). The file is written under a scratch
name at the root and moved into place only after the projection succeeded, so a failed window leaves no directory, no file and an
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

The third group is optional per file. A file of FINAL rows — a publish, or any sink stating its own current state — has the first two groups and none of the third, and that absence is what tells a reader it is looking at current state rather than at history (see "A file without ops").

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
      "file": "<table hex>-<first>-<last>.parquet",
      "key_bounds": [["org", "00000001", "00000001"], ["id", "000000000000000a", "000000000000000b"]] }

`columns` is in column order (keys first) with the Arrow type name and the
nullability the file declares. `key_bounds` is one `(column, min, max)` per prunable key
column, holding the range the file covers as hex of the values' SORTABLE bytes — big-endian
integers, the sign bit flipped for `Int64`, raw UTF-8 for strings — so one byte ordering answers
every key type. A type outside that set (a decimal, a nested shape) gets no bound, and a column
with no bound prunes nothing: an over-wide candidate list is a slow fold, a wrong one loses
rows. The manifest is written BEFORE the watermark moves,
so a consumer that sees a watermark can read a description of what it points at —
and a window is never half-described.

## What a window means

* **One generation, one vocabulary.** A window carries a single declaration and a single
  field vocabulary: the labels a column exports come from the store's intern table, and a
  store that starts fresh numbers from the beginning again, so two segments can give one id
  two names. A segment that disagrees with the window's vocabulary starts the NEXT window
  instead of joining it — one column cannot be named two things. Nothing is lost: the two
  segments become two files, each described by its own labels.
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

## A file without ops

A file of FINAL rows carries no `okm_op` column: that is a published table, and any sink that states its own current state instead of a history. It means the opposite of "no changes": its rows ARE the answer, so nothing is folded and no removal is applied. Reading one is a pass-through — no op filter, no last-`okm_seq`-per-key rule — and it is legal for a table to hold both shapes at once, because the column is optional per FILE: a change window written before the sink started publishing keeps its ops, and a union schema gives the final file a NULL `okm_op` on every row, which every reader here treats exactly like an absent column.

A fold therefore refuses a table that is already all final rows ("nothing to fold") — but only when there is genuinely nothing to fold. A table whose live files are still change files DOES fold when every key resolves away: the removals are ops, they fold to nothing, and no file replaces the files they came in. The refusal is about the ABSENCE of ops, never about the absence of rows.

## Consumer obligations

* Treat a window file as immutable once written.
* Advance the watermark only to a window that has been **committed** to the lake —
  the watermark IS the lake's commit, and the file it names is the evidence.
* Never reorder. A sink that needs a different physical layout may rewrite rows
  inside a file it owns; it may not reorder across windows.

## Folding

`okm-lake condense --out <lake>` turns a table's change files into ONE file of current
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
as the file it always was.

Deciding that costs almost nothing. Each manifest records the range its keys cover, so two
files can only share a key where those ranges overlap; a sorted pass over the first key column
marks the overlapping files, and only those have their key columns read (with a projection, so
still no payload). A file whose range is disjoint from every other file is never opened. That
is what a row-level removal costs here: deleting one key rewrites the one file whose range
holds it — copy-on-write with a range index, rather than a deletion vector pointing into files
nobody rewrote. A table with nothing to resolve is refused rather
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
becomes `put` (or `put_open` when the key never had a full row). `okm_lake::read::open_fields`
is the same decoder, exposed for anyone reading the export.

A `remove` action drops a file from the TABLE; it does not delete bytes. Replaced files stay
in `data/` for anyone reading an older version, and reclaiming them is a vacuum — a separate
question (how long history is kept) this writer does not answer yet.

## Status

`okm-lake status --out <lake> [--table <hex>] [--rows]` reports one table per block:
version, live files and bytes, change rows and removals summed from the manifests, whether the
newest file is a fold, and how many bytes a vacuum would reclaim. It reads the log, the file
sizes and the manifests — no payload — so it is cheap on a table with a million rows.

`--rows` is the exception: rows of CURRENT state means folding the table, so it is asked for
rather than paid for. A file without a manifest makes the change-row sum an undercount, and the
answer is "unknown" instead of a number that silently means "some of it".

A folded file gets a manifest too, carrying the segments of the files it replaced: provenance
survives a fold, and the status can still count what is in the lake.

## Vacuum

`okm-lake vacuum --out <lake> --keep-versions N [--dry-run]` deletes the files no kept
version can see. It is the other half of `condense`: a `remove` action takes a file out of the
TABLE and leaves the bytes in `data/`, because a reader asking for an older version still needs
them — so reclaiming them is a retention decision, and the only honest default is none.

`N` counts VERSIONS, not days: the log is the clock, and a table that folds weekly has a
different calendar from one that folds hourly. `N = 0` keeps only the current version and
deletes every file it does not read — the aggressive setting, spelled out rather than guessed.
A file's own `manifest.json` goes with it, since a manifest describes a window nobody can read
afterwards.

The log still names the deleted files, so a second vacuum finds them absent and reports nothing
— the sweep is idempotent, and it never edits the log: what the table SAID stays true, only the
bytes behind the old versions stop existing.

## File size

`DrainConfig.max_rows_per_file` (CLI `--max-rows-per-file N`) cuts a window — or a fold — into
files of at most N rows, named `<table>-<first>-<last>-p<k>.parquet`. `0` is the default and
means "as many rows as the window holds". A window that fits in one file keeps the plain name it
always had.

The limit counts ROWS, not bytes. A byte target is a feedback loop — write, measure, rewrite —
and a writer that can measure before writing would have to know how the Parquet writer will
compress; rows are what a window knows up front, and a reader's cost is bounded by rows times a
bounded row size. Row-group size inside a file is the Parquet writer's own lever, not this one.

Parts are files like any other: each carries its own manifest and therefore its own `key_bounds`,
which makes the range index finer the smaller the parts — a limit set for file size also
sharpens what a fold has to open.

## Partitioning

`DrainConfig.partition` (CLI `--partition <column>`) writes one file per partition VALUE per
window:

    <out>/<table hex>/data/<column>=<value>/<table hex>-<first>-<last>.parquet

The column has to be one of the table's KEY columns. A partition has to refine the key space:
partition by a payload column and one key could live in two partitions, where a fold can no
longer see both halves of it and no reader can be told which file holds "the" row. A window
whose partition column names something else is refused, like any other window the contract
cannot state.

Delta carries the value twice, on purpose: the directory is a naming convention a reader may
use, and `partitionValues` in the `add` action is what a reader is actually told. The directory
name percent-encodes anything outside `[A-Za-z0-9._-]` (a value is data, a path is not a place
for `/` or `=`), while the action holds the raw text. `partitionColumns` is written once in
`metaData`; a window that would change it is refused, because a reader reuses that answer.

What partitioning buys here is narrower than in a warehouse: no window is split by time, and
filters still land on Parquet statistics. What it costs is a fold that must stay inside a
partition — which it does by construction, since the partition column is a key column: keys are
not shared across partitions, so neither is a fold. And what it composes with is the key-range
index: a partition already bounds the range, and the manifest's `key_bounds` bounds it again.

Vacuum is unchanged: a path carries its partition directory, and a `remove` names the path.

## Not part of the contract

Partitioning, target file size, catalog registration, snapshot management and
retention all belong to the adapter. `okm-lake`'s only knob is
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

## Where a reader starts

A reader does not have to replay a log from version 0. A checkpoint states the log's OWN actions with the history already resolved — the `protocol`, the `metaData`, and one `add` per live file — so a reader starts at the NEWEST `_delta_log/<version, 20 digits>.checkpoint.json`, takes that state as of the version in its name, and replays only the versions AFTER it (ADR-0065). It is not a second format: the action loop is the same one, and a checkpoint only saves reading the versions it stands in for. `PublishOptions::checkpoint_every` is where this writer decides to lay one down.

Two things a sink author has to keep. A checkpoint is an optimisation, never an authority: the version files are what the table SAID, so a log with no checkpoint (or one whose artifact is gone) still reads from version 0, and a rollback of the reader is a deleted file rather than a repair. And it is written AFTER the commit it describes: a checkpoint naming a file the store no longer has turns a cheap read into a failed one.
