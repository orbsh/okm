# ADR-0034: A table has a name, and an export has the shape of a table

**Status**: Accepted (2026-10-21) — implemented in the same round: `#[ok_name]`,
`export_parquet_with_variant`, `Graph::to_record_batch_table`, `Graph::put_fields_into` /
`get_fields`, `export_reduces`.
**Date**: 2026-10-21
**Chinese**: [0034-table-names-and-export-shape.zh-CN.md](0034-table-names-and-export-shape.zh-CN.md)
**Depends on**: ADR-0007 (the Arrow bridge), ADR-0012 (field-name vocabulary),
ADR-0017 §3 (the edge ns and its slot layout), ADR-0023 (reduce presets)

## Context

ADR-0007 Phase 1 exports ONE COLLECTION: typed columns generated from the row
declaration, written to a Parquet file. Three things that a backup needs were never
in that picture, and each one is a different failure:

1. **The ns is not a name.** A file whose columns are `since_year` and `weight` says
   nothing about being an `Employment` edge — the only address a collection has is a
   16-bit number that is meaningful inside the store and nowhere else. Whoever reads
   the file, and whoever restores it, needs the table's NAME.
2. **Edges are not documents.** An edge lives in the edge ns's own entry shape
   (`EdgeKey` + `EdgeBody`), so the bridge — which reads `Document` collections — has
   nothing to read. A backup of a graph was therefore a backup of the nodes only, and
   a backup that silently omits the facts is worse than one that refuses.
3. **The open half of a row is not a column.** A document's dynamic segment (0x01)
   and an edge's open attributes are not declared fields, so an export built from the
   declaration loses them. Reduces are not rows at all: an accumulator lives in its
   own slot family and has no document.

## Decision

### 1. A table declares its name beside its ns

`#[ok_name("messages")]` on the row, emitted as `Document::TABLE_NAME` (default
`""`). Two rules:

- **Empty means unnamed, and an export REFUSES it.** Spelling the Rust struct's name
  would be a second vocabulary invented at export time — and a struct name is not a
  table's name.
- **Renaming moves no data.** The ns is the address; the name is what the table is
  called outside the store. A rename touches the declaration and the catalog row,
  never a stored byte.

The name↔ns correspondence is a **catalog**: one small metadata table, `(name, ns)`
per table. Pure analysis does not need it — the export's columns are already labeled.
Import does, and so does any runtime question of the form "what is ns 41 called".

### 2. The shape of an exported table

Key fields (declaration order) → declared payload fields → **one `variant` column
holding the dynamic segment**. Columns keep the declaration's order and types;
`Vocab` fields stay labels. The variant column is the analytics twin of the dynamic
path: nested objects and arrays recurse, names resolve through the field-name
vocabulary, and a row without a dynamic segment is `null` in that column.

Indexes are NOT exported. They are derived state, rebuilt by `put` on import — a
second copy in a file would be a second thing to keep consistent.

### 3. The edge set is one table, with (table, key) endpoints

An edge table's columns start with `[src_table, src_pkey, dst_table, dst_pkey,
predicate]`, then the edge declaration's fields, then the `variant` column. The
node reference IS a (table, primary key) pair, so the export writes it as one.

`src_pkey` / `dst_pkey` are the RAW key bytes. One edge table spans node
collections; decoding another table's key into that table's fields would need that
table's declaration, which the edge ns does not carry. `name_of` is supplied by the
assembler (it knows the catalog) — an ns it does not know exports as NULL rather
than a guessed name.

### 4. The edge's open attributes belong to the graph

`Graph::put_fields_into(batch, edge_id, &map)` / `get_fields(edge_id)` own the 0x01
segment, keyed `[ns][0x01][edge_id]`, names allocated in the field-name vocabulary.
A rewrite (`link_into`) DELETES the previous segment in the same batch: attributes
are derived from the assertion that was just overwritten, so inheriting them would
attach the old claim's conditions to the new one.

Before this record the segment was written by the consumer (Krystallizer), because
the slot layout was documented but no method existed. A format with no owner is a
format with N implementations; the export is the second consumer, so the segment got
a home.

### 5. Reduces are a derived table of their own

`(table, reduce, group, value)`, one row per reduce entry, group bytes empty for a
whole-table group. A reduce is not a row and must not be squeezed into one; but it
must travel with a backup, because **a restored table whose watermark is missing
hands out ids it already used** — a restored log that then appends over an existing
`seq` is a silent corruption, not a cosmetic gap.

## Alternatives considered

- **A consumer-written dump (JSON/CSV).** Rejected: a second description of the same
  rows, maintained beside the declaration, carrying a consumer's field names into
  whatever reads the backup.
- **One archive per backup.** Rejected: it hides which table failed and adds a
  container parser to the restore path. One table, one file means a partial restore
  is a table.
- **Decoding endpoint keys into the node tables' fields.** Rejected: an edge table
  does not know its members' declarations, and a global registry of key layouts would
  be a second source of truth for the ns vocabulary.
- **Exporting indexes.** Rejected: derived state, rebuilt on import.

## Consequences

- A backup is readable without the store: labels, names, and typed columns.
- Restoring is a real operation with real questions — which ns (the request names the
  table, and the schema is checked against that table's declaration, because the
  import reads columns by INDEX), replace vs merge, and a `Vocab` label the
  destination vocabulary cannot name (the import refuses rather than guessing).
- The watermark rule from §5 becomes a restore obligation: after importing a table,
  its reduce rows must be replayed or recomputed before the next write.
- A consumer that names its tables in the catalog gets its facts in the backup; one
  that does not still gets nodes and documents, and the catalog says what is missing.
