# ADR-0029: Path Indexes — Declared Paths over the Schema-Free Zone

**Status**: Accepted (2026-10-14) — implementation landed with this decision
**Date**: 2026-10-14
**Chinese**: [0029-path-index.zh-CN.md](0029-path-index.zh-CN.md)
**Depends on**: ADR-0012 (field-name vocabulary), ADR-0016 (4-byte head, slot
segments), ADR-0020 (range scan), ADR-0022 (dynamic access methods),
ADR-0025 (two schema carriers)

## Context

The dynamic segment is the schema-free zone: nested objects and arrays live
there as nTLV frames, and field names are interned through the per-table
field-name vocabulary (ADR-0012). Access methods over that zone were in
practice limited to *schema-declared* fields — `AccessMethod::plain` names
fields `find_field` resolves in the schema (key/hot/cold), so a location
inside a nested object, or an array element, had no declarative form. A host
could always write a `Func` index (the callable receives the decoded document
and returns encoded bytes), but that pushes byte-layout knowledge and the
value type into every host language, once per index.

What already existed, and is NOT new here:

- the field-name vocabulary gives every dynamic field a **fixed-width
  handle** (u16, compact lane: `< 0xFF` one byte, `0xFF` escapes to a u16 BE
  tail); nested object field ids ride the same vocabulary;
- `DynamicCollection::declare_index` declares access methods at runtime, with
  caller-allocated slots;
- index entries already share one byte layout —
  `[ns 2B][slot 2B][indexed segment][pkey]`, value = the includes segment;
- multi-entry fan-out already exists as a shape (`Func` returns
  `Vec<Vec<u8>>`).

## Decision

**A path is a declared value with its own component vocabulary; a path index
declares a path plus the value type its entries are ordered by.**

1. **`Path` = a sequence of components**, three kinds:
   - `Field(name)` — an object key, interned through the field-name
     vocabulary (a *stable identity*);
   - `Index(u32)` — an array position (*data*: never interned, and not stable
     across mutations, since an insert shifts every later position);
   - `Key(bytes)` — an object key carried literally, no interning.

   `Field`/`Key` versus `Index` are two namespaces and two walker operations
   (map lookup vs subscript), not two spellings of one thing. Collapsing them
   into one integer would niche an id space behind an implicit convention —
   the failure mode ADR-0001's ns direction bit was rejected for.

2. **The canonical byte form is self-delimiting and carries NO leading
   component count**:

   ```text
   Field → [0x01][handle]         handle: u16, compact lane
   Index → [0x02][u32 BE]
   Key   → [0x03][len][bytes]     len: compact lane
   ```

   Without a count:
   - a path is a byte prefix of every extension of itself, so "scan everything
     under this path" stays ONE prefix scan;
   - **a count would make DEPTH the primary sort key**, so a subtree's entries
     would stop being adjacent. This is the load-bearing reason, not a
     micro-optimization: with `[2][a][b]` compared against `[1][a]`, every
     one-component path sorts before every two-component path;
   - an array wildcard needs no third component kind — it is the ABSENCE of
     `Index`, and `[a]` stays a byte prefix of `[a][0][b]`;
   - depth is derivable by walking; a count would only buy O(1) depth.

3. **One slot per declared path, and the path does NOT ride the entry key**
   (the slot is the path's identity). The entry key stays
   `[ns][slot][value][pkey]`, so the value's width is the declaration's and
   entries stay statically cuttable — no "walk the path, then slice the
   value" regime, which would be a third key layout in a layer that has
   exactly two (static offsets, and the trailing variable-width tail).

4. **The value type is declared, never inferred.** The entry segment is byte
   ordered; the primary document gets its type from `FieldSchema`, while a
   path has no schema field to consult, so the declaration carries it. Only
   types with a declared encoding are accepted (U8/U16/U32/U64/FixedBytes/
   Str) — a wrapper-like type is rejected AT DECLARATION time rather than
   failing at every write.

5. **Arrays fan out at write time**: one entry per resolved value. A value
   whose variant does not match the declared type produces no entry and no
   error (a heterogeneous array is normal data — the "absent attribute values
   are empty results" rule), and so does a missing path.

6. **Path indexes attach to the dynamic-segment write path**
   (the whole-document writes `merge` / `replace` / `purge`, ADR-0035), never to the primary document path. The
   primary document holds schema-declared fields only, where a depth-1 path is
   exactly what `Plain` already indexes; nested content exists only in the
   schema-free zone. Exactly ONE maintainer per entry set: the primary path's
   `index_entries` skips `Path` declarations on purpose.

## Consequences

- The declaration surface is one constructor: `AccessMethod::path(slot, path,
  value_ty)`.
- Overwrite sweeps the old entry set by recomputing it from the stored dynamic
  map (entries are keyed by resolved values), so a changed value leaves no
  dangling entry — the rule the primary path already follows.
- A path index costs the same bytes as any index (no path in the key); the
  write amplification is one entry per resolved value per declared path.
- `Path::encode`/`decode` cover the form paths take where they are DATA
  (declaration carriers, exported schemas). They are **table-scoped by
  construction**: handles come from that table's vocabulary, so a path
  decodes only against the table it was written for.
- The query surface is unchanged: the leftmost-prefix scan treats a path index
  like a func index (the segment width is the caller's business, so any
  prefix — including empty — is legal), and ADR-0020's range scan covers
  ranges over the value.
- Deliberately out of scope: a universal nested-value index (one slot for ALL
  paths). It would put variable-width path bytes into the entry key and force
  a walk-then-slice key regime; declaration is registration, so per-path slots
  stay the shape.

## Update (2026-10-14, landing)

Recorded because the draft and the code differ, so a later session reads the
difference as a refinement rather than a bug:

- The draft's component sketch wrote `Field(u32)`; the vocabulary's handle is
  **u16**, and the encoding reuses the vocabulary's own compact lane
  (`< 0xFF` one byte, `0xFF` + u16 BE) instead of inventing a width. With
  fixed-width handles the only variable-width component is `Key`.
- The draft left the attachment point open (an interface question: the plan
  surface's whole-document write signature would have had to grow an `old fields`
  argument). The landing attaches path indexes to the dynamic-segment write
  path and reads the old map there only when a path index is declared, so the
  plan surface and every binding call site are untouched.
- Path entries carry an EMPTY value segment: `includes` is not supported for
  the `Path` kind in v1 (an includes request would need the same "one
  maintainer" analysis).
- All three halves landed 2026-10-31. **Backfill** is
  `DynamicCollection::rebuild_faces(chunk)`: it sweeps every declared method's
  slot and re-derives, path entries included, then stamps — the same sweep →
  derive → stamp order the typed verb uses, and the same reason for it (an entry's
  key encodes the value it indexes). **The registry** is the meta row:
  `ensure_layout` records one name per path face (`path:0x3001` → the canonical
  text), and `path_registry()` reads it back for a process that never saw the
  declaration — the slot is the face's identity, so without the record a reader
  sees a number and nothing else. Drift detection is not the registry's job: the
  fingerprint (ADR-0048) covers the components and the value type, so a changed
  path answers `Stale` on its own. **The binding** is
  `Collection.add_path_index(slot, path, value_ty="str")` in `okm-python` — a pure
  declaration needs no callable bridge — together with the write path it needs:
  `merge` now routes an undeclared name into the dynamic segment (typed by the
  Python value), because a path index over declared fields would be what `Plain`
  already covers.