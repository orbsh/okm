# ADR-0036: A value too big for the row is its own table

**Status**: Rejected — `#[ok_offrow]` is not implemented; the second table is the decision
**Date**: 2026-10-23
**Chinese**: [0036-value-too-big-is-its-own-table.zh-CN.md](0036-value-too-big-is-its-own-table.zh-CN.md)

## Context

A payload carries every declared field in one value under
`[ns][0x00][pkey]`: fixed-width fields contiguous in a hot segment, the rest as
TLV frames in a cold segment (ADR-0004, ADR-0006). One column that is orders of
magnitude larger than the others — a blob, a rendered document, an embedding —
therefore has two costs: every read of the row materializes it, and every write
of ANY field re-encodes it.

The proposal (`#[ok_offrow]`, approved in PLAN 2026-10-22 and withdrawn the next
day): a declared field carrying the annotation leaves the payload and lives
under its own key, so a named-update path can write that column without
rewriting the row.

- **Layout**: one fixed face for every off-row field,
  `[ns][0x0C][pkey][ordinal u16 BE]` → the value bare; `ordinal` is declaration
  order among off-row fields, append-only. One prefix scan
  `[ns][0x0C][pkey]` returns every off-row field of one document.
- **Type surface**: an off-row field does not appear in the generated struct,
  so the default access paths (`get` / `put`) never pay for it. The derive
  emits, per document, a tag enum (one variant per off-row field, discriminant =
  ordinal) plus ONE field-level accessor taking that tag; the whole-document map
  verbs route a name to the column. A post-decode hook would have filled the
  struct had the field stayed in it.

Both halves are sound in isolation — the diagnosis (whole-document access
nullifies the feature) and the selector shape (one tag enum rather than one
method group per field) are the right answers to the problem as posed.

## Decision

**Not implemented, and no replacement mechanism inside the row codec.** The job
is done by a second table whose key type is the parent's own key type:

```rust
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]          // the SAME key type: byte-identical key payload
#[ok_ns(44)]                // one ns of its own — the whole cost
#[ok_name("post_payloads")]
pub struct PostPayload { pub mime: String, pub body: Bytes }
```

Read one column with `payloads.get(&key)` (one point read, the row never
decoded), write one with `payloads.put(&key, &payload)` (one row write, the hot
row never re-encoded), write the pair with `save_with_prev_into` on both
collections into one `MemBatch` and one `commit_batch` (ADR-0003, ADR-0031), so
the pair is one commit. The modelling case is in
[MODELING](../MODELING.md#a-value-too-big-for-the-row-its-own-ns-the-same-key).

Three reasons, in the order they decided it:

### 1. The complexity is large, the functional gain is small

Off-row adds a fifth storage location to the row model, and every consumer of
the row layout has to learn it: a third dimension on `FieldDesc` (payload hot /
payload cold / off-row), a third bucket in `FieldSchema`, a name that
`split_names` must route away from the dynamic segment, branches in `fetch` /
`merge` / `replace` / `purge` / `delete`, a trait associated type and a const
table for the tag enum, four derive walks that must skip the field, an unused
post-decode hook, a write-side entry generator, and the dynamic facade's plan
surface. Each of those is a place where the annotation has to be remembered
forever, and each is a place where getting it wrong is silent — a field that is
declared but not encoded, or a name that falls into the wrong segment.

What that complexity buys, over the side table, is one thing: a whole-document
read costs one round trip instead of one extra point read. That is the read the
split exists to avoid paying in the first place.

### 2. It collides with the interface semantics

ADR-0035 landed one day earlier with the rule that the document surface is one
name-keyed map and one write is one commit. Off-row breaks that at five points:

- `R` stops being the document. A `Document` whose declared fields are not in
  it is a declaration that disagrees with its own type.
- `fetch` must either omit a declared column (a projection that silently drops a
  field — the lie ADR-0035 was written to remove) or pay a scan on every use.
- `replace` can no longer mean "the map IS the document", so the documented
  identity `replace(&empty_map) == purge` fails: either off-row data survives a
  whole-document replacement, or `replace` gains a clear-it pass over a face it
  otherwise never touches.
- `put(&R)` stops meaning "this document's state": an off-row column outlives
  it, which is correct behaviour that the type no longer expresses.
- `purge` reads the stored row and, when one exists, the segment; an off-row
  face would add a prefix scan to every purge.

And the field-level accessor is a second selector for a value the name-keyed
map already selects: a tag enum cannot carry the field's type, so the accessor
returns one erased currency (`DynamicValue`, or bare frame bytes) and the static
type of a declaration checked at compile time becomes a runtime check at the
boundary. If the mechanism had been built, the tag enum was still the right
selector — it gives exhaustiveness over the document's columns and pins the
ordinal in the code — but it is a second surface on top of the one the project
had just unified.

### 3. The simple alternative covers it

A second table with the parent's key type:

| | off-row annotation | second table |
|---|---|---|
| key | `[ns][0x0C][pkey][ordinal 2B]` | `[ns'][0x00][pkey]` — same length as the parent's |
| read one column | 1 point read | 1 point read |
| write one column | 1 key write | 1 row write |
| pair write, atomic | 1 commit | 1 commit (ADR-0003 batch) |
| pair delete, atomic | 1 commit | 2 commits today — public `delete_from(batch)` missing |
| indexable / reduce-grouped | forbidden (fixed-width keys) | allowed |
| several columns per row | one frame per field, one face | one row, any shape |
| own `#[ok_layout]` version | no | yes |
| export | bridge change (carry the 0x0C slot) | free — export and import are per `(K, R)` (ADR-0034) |
| `R` stays complete | no | yes |
| new mechanism surface | 9 places in the row model | none |

The only row where the annotation wins is a whole-document read that also needs
the payload: one round trip instead of two. A side row's presence is also a
better "has payload" flag than an absent key: it is one state, not an absence.

## Why not

- **Keep the annotation AND allow the side table.** Two ways to say "this value
  lives elsewhere" must then be kept consistent with each other forever — the
  same defect that retired the six slot-carve-out verbs a day earlier.
- **Annotate only the payload, not the access surface** (leave the field in `R`,
  fill it with a post-decode hook, write all columns on `put`). That is the
  version with no API change, and it is exactly the one the proposal rejected
  for a reason that holds: `put(&R)` then writes the same bytes under a
  different key, so the annotation buys nothing at all.
- **A counting family for the off-row face** (`[ns][0x6n][pkey]`). Rejected
  inside the proposal already: it buys a column-wide scan no consumer needs and
  pays #fields point reads per document.
- **Reuse the dynamic segment** (slot 1) for the big column. Slot 1 is rewritten
  whole per document like the row is, so it isolates nothing; and it is the
  schema-free zone, not a place for a declared, typed column.

## Consequences

- No code, no schema change, no migration. `FieldDesc`, `FieldSchema`, the four
  document verbs, the derive walks and the dynamic plan surface stay as they are.
- The modelling rule is documented as a case in MODELING (en / zh): same key
  type, own ns, one batch for the write pair, and the note that the delete pair
  is the caller's (nothing cascades in this model — junctions are the same
  shape).
- Two declarations exist where one did. The compiler holds the pairing: the side
  table reuses the parent's key type, so the key payload is byte-identical by
  construction and only the ns and the table name differ.
- A public batchable removal (`Collection::purge_into(&mut MemBatch, &K)` over
  the existing private `delete_into`) is the one surface item this leaves
  behind; it is recorded in PLAN, not here.
- It would reopen on evidence, not on taste: a payload in the multi-megabyte
  range where the whole-document read must stay one round trip, and where the
  side table's extra point read is measured as the bottleneck. Nothing in the
  current consumers is near that.