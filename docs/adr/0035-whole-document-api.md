# ADR-0035: One whole-document surface — `view` / `merge` / `replace` / `purge`

**Status**: Accepted (2026-10-22) — landed with this decision
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
| `delete(&key, &R)` | static segment + faces + ledgers | 1 | unchanged |
| `view(&key) -> Option<BTreeMap<String, DynamicValue>>` | merged projection of both slots | — | `get_document` + `get_fields` |
| `merge(&key, &map)` | declared names → row RMW; every other name → that name in slot 1 | 1 | `put_document` (merge half) + `put_fields` |
| `replace(&key, &map)` | row rebuilt from the map; slot 1 = exactly the map's non-declared names | 1 | — (stronger than `put_document`) |
| `purge(&key) -> bool` | both slots removed | 1 | `delete_document` + `delete_fields` |

1. **The static segment speaks the struct.** `get` / `put` / `delete` keep their
   contracts. `put` in particular keeps "never cleans the previous values'
   faces; `delete(old row)` owns that" — `index_test.rs` locks it deliberately.
2. **The whole document speaks one name-keyed map.** `view` projects both slots;
   a dynamic-only document still views, because the document is the PAIR of
   slots, not the primary entry.
3. **`merge` touches only the names it is given.** A declared name is a row
   read-modify-write through the semantic path (`save_with_prev_into`), so the
   previous values' faces ARE removed and the reduce ledger compensates. Any
   other name overwrites that name in slot 1 and leaves the rest alone. A map
   with no declared name never writes the row.
4. **`replace` treats the map as the whole document.** Declared names rebuild
   the row; a declared field the map does not name takes `from_map`'s default
   (the payload is positional — a half row cannot be encoded). The dynamic
   segment is exactly the map's non-declared names, so `replace(&empty_map)` is
   `purge`. A map with no declared name leaves the static segment absent,
   deleting the row if it existed.
5. **`purge` removes both slots** in one commit and answers whether there was
   anything to remove.
6. **Every write is one engine commit.** Row ops and the slot-1 entry accumulate
   in one `MemBatch` and reach the engine once, so a crash lands all of them or
   none.
7. **The six old names are retired with no aliases** (pre-1.0; consumers ride
   the git dependency and are swept in the same session).
8. **`Graph`'s edge-attribute face drops the slot carve-out vocabulary too**:
   `get_fields` → `attrs`, `put_fields_into` → `replace_into`. The function
   replaces the segment outright, so naming it `merge_into` would describe
   behaviour it does not have; an edge's typed half is the payload, not a map,
   so `view` would over-claim as well.

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
- **Let `merge` keep `put`'s face contract (leak the old entry).** A
  whole-document entry point has no old row in hand, so it cannot perform the
  cleanup that contract delegates to the caller. `DynamicCollection::put`
  already cleans; two entry points must not have two face lifecycles.
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
  host-language dict) writes with `merge` / `replace` and reads with `view`.
- `view` answers a dynamic-only document; the old `get_document` returned `None`
  whenever the primary entry was absent, hiding the segment.
- Cost: `view` costs two point reads (row + segment), `merge` one more for the
  row RMW.
- `okm-dynamic` keeps its own `DynamicCollection` surface (it does not wrap the
  typed assembly point) and gains the same three write verbs on the whole
  document; its plan surface will emit one op list per document, so a remote
  plan stays one replay.