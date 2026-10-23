# ADR-0035: One whole-document surface — `fetch` / `merge` / `replace` / `purge`

**Status**: Accepted (2026-10-22) — landed with this decision. **Updates 2026-10-23**: the `okm-dynamic` half landed (`fetch` / `merge` / `replace` / `purge`, the slot-1 trio retired, and a whole-document plan surface); **the removal verbs collapsed to `purge`** — `delete` / `delete_by_pkey` are retired, since a row cannot outlive its segment and a row-only delete was a way to leave one dangling; **`view` became `fetch`** — the read door of the document surface (a *view* is a projection of a row, and the map is not a projection any more, it is the document); **and the dynamic side lost `get` / `put`** — with no struct to speak, its `put` could only mean "the static segment", a semantic invisible in the signature, and `merge` produces the identical op list. The consumer sweep is what remains.
**Date**: 2026-10-22
**Chinese**: [0035-whole-document-api.zh-CN.md](0035-whole-document-api.zh-CN.md)

## Context

The document API was split along the STORAGE layout instead of the caller's
intent: slot 0 had `put` / `get` / `delete`, slot 1 had `get_fields` /
`put_fields` / `delete_fields`, and a whole-document write (`put_document`)
routed by name across both. Three consequences followed.

1. **The caller had to know the layout.** Whether a name lives in the declared
   struct or in the dynamic segment is a storage fact. A caller holding an
   external map had to know its own schema before it could pick an entry point.
2. **A whole-document write cost two commits.** `put_document` was `put` +
   `put_fields`, i.e. two engine commits, so the two slots could tear across a
   crash — the operation was not atomic with itself.
3. **`put_document`'s name lied.** It was called put but behaved as a merge:
   declared fields the map did not name kept their stored values. Nothing in
   the API ever said "the map is the document".

Also observed while implementing: the typed side has no per-field setter at all
(the derive generates none), so "update one static field" already meant
read-modify-write of the whole struct. Whole-struct replacement is the only
typed write there is.

## Decision

The surface splits by VALUE SHAPE rather than by slot.

| operation | covers | commits | replaces |
|---|---|---|---|
| `get(&key) -> Option<R>` | static segment (typed) | — | unchanged |
| `put(&key, &R)` | static segment + faces + ledgers | 1 | unchanged |
| `fetch(&key) -> Option<BTreeMap<String, DynamicValue>>` | the whole document (both slots merged) | — | `get_document` + `get_fields` |
| `merge(&key, &map)` | declared names → row RMW; every other name → that name in slot 1 | 1 | `put_document` (merge half) + `put_fields` |
| `replace(&key, &map)` | row rebuilt from the map; slot 1 = exactly the map's non-declared names | 1 | — (stronger than `put_document`) |
| `purge(&key) -> bool` | both slots removed | 1 | `delete_document` + `delete_fields` |

1. **The static segment speaks the struct.** `get` / `put` keep their
   contracts. `put` in particular keeps "never cleans the previous values'
   faces" — `index_test.rs` locks it deliberately — and nothing can clean them
   from a held old row any more: the overwrite path is `merge` / `replace`,
   which sweep the STORED row's faces. `put` is the fresh-row fast path.
2. **The whole document speaks one name-keyed map.** `fetch` reads both slots
   as one map; a dynamic-only document still fetches, because the document is
   the PAIR of slots, not the primary entry. It is the read half of `merge` /
   `replace` / `purge` — reading and writing speak the same shape.
   **This is the only read door on the dynamic side**: `get` / `put` are the
   STRUCT door, and a schema-as-data collection has no struct, so there they
   could only mean "the static segment" while wearing a whole-document name.
3. **`merge` touches only the names it is given.** A declared name is a row
   read-modify-write through the semantic path (`save_with_prev_into`), so the
   previous values' faces ARE removed and the reduce ledger compensates. Any
   any other name overwrites that name in slot 1 and leaves the rest alone. A map
   with no declared name never writes the row — nor reads it: the stored state a
   verb touches is decided per arm by the schema, which costs no read. The
   field-name vocabulary is the segment arm's too: it is loaded only when the map
   carries a non-declared name (a declared-only patch on a table with no open
   fields would otherwise pay two prefix scans and a cursor read for nothing).
4. **`replace` treats the map as the whole document.** Declared names rebuild
   the row; a declared field the map does not name takes `from_map`'s default
   (the payload is positional — a half row cannot be encoded). The dynamic
   segment is exactly the map's non-declared names, so `replace(&empty_map)` is
   `purge`. A map with no declared name leaves the static segment absent,
   deleting the row if it existed. The stored row is therefore ALWAYS read —
   rebuilt means its faces must be swept, absent means it must be deleted, and
   only the stored row says which.
5. **`purge` removes both slots** in one commit, answers whether there was
   anything to remove, and carries the Delete event for subscribers (the signal
   moved with the verb). It is the ONE removal verb: a document is the pair of
   slots, so "delete the row, keep the segment" has no sound use.
6. **Every write is one engine commit.** Row ops and the slot-1 entry accumulate
   in one `MemBatch` and reach the engine once, so a crash lands all of them or
   none.
7. **The six old names are retired with no aliases** (pre-1.0; consumers ride
   the git dependency and are swept in the same session), and so are `delete`
   and `delete_by_pkey`.
8. **`Graph`'s edge-attribute face drops the slot carve-out vocabulary too**:
   `get_fields` → `attrs`, `put_fields_into` → `replace_into`. The function
   replaces the segment outright, so naming it `merge_into` would describe
   behaviour it does not have; an edge's typed half is the payload, not a map,
   so `fetch` would over-claim as well.
9. **The ledger and the enumeration follow the ROW.** A segment-only document
   folds nothing — `Count` / `Sum` / `HighWater` are written by the row arm — and
   one `merge` folds ONCE: the fold belongs to the arm, not to the number of names
   in the map, so a patch writing both halves does not count twice. Enumeration
   has two modes for the same reason the document does: `scan_keys` enumerates
   rows (one prefix scan), `scan_document_keys` enumerates documents (that scan
   plus a key-only scan of the segment slot, merged in memory, no point read);
   materializing is `fetch`'s job, so a key becomes a document with a plain
   `map`. The Parquet backup carries both halves as well — the typed columns plus
   the `variant` column for the open half — so a segment-only document restores
   as one instead of gaining an invented row of defaults.

## Why not

- **Keep the slot-split verbs alongside the new ones.** Two ways to write one
  document, one of them misnamed (`put_document` behaved as a merge) and half
  the atomicity. The cost of keeping them is that every future reader has to
  learn the layout before choosing.
- **Collapse `put` into the map API too (one write verb).** The typed path is
  the fast path: a struct-shaped write allocates no map, and `put(&R)` is what
  every consumer already holds. Taxing it to unify spelling serves only the
  external-data entry point.
- **Generate a setter per declared field.** One API surface per field (a
  1000-field document becomes 1000 methods), and each setter is a
  read-modify-write of the whole payload anyway — exactly what `merge` does with
  a name. Struct-in / struct-out is smaller and faster.
- **Let `merge` keep `put`'s face contract (leak the old entry).** `put` is the
  one write that does not read the row first, so it is the only one that can
  leave a stale face; the whole-document verbs read the stored row and sweep it.
  Two entry points must not have two face lifecycles — hence `put` is named for
  what it is (the struct door, the fresh-row fast path) and carries no
  whole-document claim.
- **Reject a value whose kind mismatches the declaration here.** `from_map`
  currently falls back to the default (`_ => #dflt`). Making that a named panic
  is right, but it is a derive change with its own blast radius — queued as a
  separate item rather than smuggled into an API rename.

## Consequences

- One commit per semantic write: a crash can no longer leave the primary entry
  landed while its faces and ledgers are missing. `write_commit_test` locks the
  op counts — 3 for an insert, 4 for an overwrite, +1 when a previous face must
  be deleted, +1 per slot-1 entry.
- Callers stop choosing slots. An external map (HTTP body, decoded Parquet row,
  host-language dict) writes with `merge` / `replace` and reads with `fetch`.
- `fetch` answers a dynamic-only document; the old `get_document` returned `None`
  whenever the primary entry was absent, hiding the segment.
- Cost: `fetch` costs two point reads (row + segment) plus the vocabulary
  lookups its dynamic names need; `merge` adds the row read only when its map
  names a declared field, and loads the vocabulary only when it names a
  non-declared one.
- The dynamic side has ONE read door and no `get` / `put`. Its `put` was a
  `merge` restricted to declared names — the same op list — so the restriction
  was only visible to whoever read the docs, and a mistyped name was either
  rejected or turned into a dynamic name with no way to tell which from the
  signature. It cost no read: each verb reads per ARM, not per verb — a map of
  dynamic names never reads the row, a map of declared names never reads the
  segment, both kinds read both. `put` read the row it was about to rewrite,
  which is exactly what the declared arm does, so the removed guard is the only
  thing that went with it. `scan` stays — it is an access-method read, not a
  point read.
- `okm-dynamic` keeps its own `DynamicCollection` surface (it does not wrap the
  typed assembly point) and carries the same verbs on the whole document —
  `fetch` / `merge` / `replace` / `purge` — with its replay committing one
  `MemBatch`. Its plan surface emits one op list per document (`plan_merge` /
  `plan_replace` / `plan_purge` → `PlannedWrite`: row ops, segment ops and the
  path-index sweep), so a remote plan stays one replay.