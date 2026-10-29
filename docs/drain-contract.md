# The drain contract

What `okm-drain` promises a sink, and what a sink promises back. Written once so
that every adapter (this repo's, and a third party's) speaks the same dialect
instead of inventing one per warehouse. ADR-0045 is the decision; this file is the
interface that follows from it.

## The artifact

One **window** is one Parquet file:

    <table hex>-<first segment>-<last segment>.parquet

written beside `okm-drain`'s watermark for that consumer. Columns are:

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

Field ids `1..N` are stamped in column order, so a reader that identifies columns
by id needs no name mapping.

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
  a sink that cannot express deletes is safe by construction.
* **The dynamic segment is not in a window yet.** Ops on the dynamic face are
  counted in the report (`segment_only`) rather than dropped in silence. When it
  lands, it travels as okm's published mapping — `Binary` as base64, `Int`/`UInt`
  as numbers, lists and maps as arrays and objects — or natively as a variant type.
  A sink that invents its own mapping splits the ecosystem; this paragraph is the
  reason it must not.
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
retention all belong to the adapter. `okm-drain`'s only knob is
`DrainConfig::max_segments` — how much history one window carries — because that is
also how much history a query must merge with the hot store.