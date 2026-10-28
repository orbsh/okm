# 0044 — Keys and Rows Are Separate Declarations

> **Languages:** [English](0044-keys-and-rows-are-separate-declarations.md) (primary) · [中文](0044-keys-and-rows-are-separate-declarations.zh-CN.md)

**Status:** Accepted (2026-10-28). Landed in the declaration surface
(`Collection<S, K, R>` with `R::Key = K`, `#[ok_ref]`, `Document::collection`)
and locked by `okm-core/tests/key_row_split_test.rs`.
**Date:** 2026-10-28

## Context

Every table has two halves with different jobs. The **key** is the address:
fixed width, ordered bytes, composed once, scanned by prefix. The **row** is the
value: a declared payload, a dynamic segment, indexes, embeds. The code declares
them as two independent types — a `#[derive(KeyEncode)]` struct and a
`#[derive(DocumentEncode)]` struct that names its key with `#[ok_ref]` — and
`Collection<S, K, R>` carries both, with `R::Key = K` pinned at compile time.

Two consequences kept being argued case by case instead of stated once:

- **The side table** (ADR-0036: "a second table whose key type is the parent's own
  key type"). It works only because the key type is a standalone declaration that
  can be reused verbatim; the reason was never written down.
- **A name that appears in both halves.** The storage shape settled it — the
  classifier walks the payload declaration and the key's bytes never enter the
  map — but nowhere said the two same-named fields are unrelated, so it read like
  an accident to be prevented.

Alternatives exist and are in use elsewhere: embed the key in the row (one struct,
the row carries its own address), or let the ns imply the key (no key type at all).

## Decision

**1. Two declarations, one binding.** The key type stands alone — declaration
order, big-endian, fixed width, its own `FIELDS` name list. The row type *names*
it (`#[ok_ref(K)]` → `Document::Key = K`) and contains no key bytes of its own.
`Collection<S, K, R>` carries both; `Document::collection(store)` reads `K` off `R`
so a call site never restates the key type (ADR-0003).

**2. One key type may address many tables; one table has exactly one key type.**
A side table is a second row type over the parent's key type, in its own ns. The
key payload is byte-identical by construction — the two primary keys differ only in
the ns bytes — which is precisely what makes ADR-0036's side table "the same key,
one more point read" instead of a second key space.

**3. The same name may appear in both halves, and the two fields are unrelated.**
Nothing dedupes them, nothing errors, nothing compares them on a read:
- the key's bytes never enter the map the whole-document surface returns (`fetch`
  = payload fields, then the dynamic segment);
- the classifier is the PAYLOAD declaration, so a name that names a declared
  payload field is a row write, and the key plays no part in that decision
  (ADR-0012);
- a payload copy that disagrees with the key is not corruption: the key is the
  address, the payload field is data.

**4. Nothing reconciles the copies, and nothing should.** Denormalizing a key field
into the row is a legitimate modelling choice — for an index, or to read the value
without re-deriving it from the key. Checking equality on the read path buys
nothing the address does not already guarantee.

## Where this differs from SQL — the split lives in the key

A relational engine is row-first. The row is the unit, the engine owns its
identity, and an oversized value goes off-row INSIDE the engine (overflow pages,
TOAST), addressed by the row's own id with a chunk ordinal appended after it. The
schema never learns that it happened: one table to the modeller, two physical
regions to the engine.

okm is key-first. A table IS a key space — there is no row outside a key — so the
same split is a modelling act (ADR-0036) and it lands on the address instead of in
the engine: the side table's ns becomes the LEADING bytes of the primary key, and
the key payload stays byte-identical after it.

Same idea as off-row storage, with the shard segment on the other end of the key:

```text
SQL / off-row    [ row id .................. ][ chunk 0, 1, 2, … ]   split appended
okm / side ns    [ ns 2B ][ slot 1B ][ key payload …………… ]           split prepended
```

Everything that follows from the inversion is visible at modelling time:

- **The split is a declaration, not a storage knob.** It gets its own ns, its own
  faces, its own commit, and it is queryable as a table. The relational version is
  invisible and unqueryable as anything but the parent row.
- **Both halves keep the same bytes.** The key payload is unchanged, so the
  parent's prefix structure still orders the side table: a scan by `org_id` over the
  side table is the same physical range as over the parent.
- **Nothing joins them back.** A reader that wants "row plus side value" does two
  point reads (ADR-0036). No plan can silently turn that into a join — the
  composition is the application's.
- **The engine never decides for you.** There is no automatic spill at a size
  threshold, so "when to split" is a modelling decision (MODELING, "A value too big
  for the row"), not a page-layout accident.

That is the core of the KV-vs-relational difference here: SQL hides the key inside
the row, okm puts the key — and therefore the table, and therefore the shard — at
the front of the address, and makes it the modeller's.

## Rejected

**Key embedded in the row.** One struct looks cheaper: no second type, no
`#[ok_ref]`. But the row would then carry its address, so every write has two
sources for one truth, every side table stores a copy, and the key's width becomes
a payload property — while keys must stay fixed-width and payloads may end in a
`Str`. The side-table form dies outright: a second table would have to restate the
parent's key fields and could drift from them in bytes.

**Implicit key from the ns.** Saves the type entirely and makes the key shape a
convention. It loses the structure that makes keys useful: no prefix dimensions
(`org_id` → `user_id`), no key type to share between tables, no compile-time check
that two tables agree on the address, and the ordering rule becomes a comment.

**Deduplicate or reject the same name.** Dropping the payload copy deletes data the
row may want to index; refusing it makes a denormalized copy unrepresentable. Both
charge a real modelling choice for a collision that costs nothing.

## Consequences

- `Collection<S, K, R>` has three parameters even though a call site rarely writes
  them. That is the price of the split; ADR-0003 is where hiding it is decided.
- The same-name rule is a compile-time silence, not a check. Drift between the two
  copies is possible, harmless for addressing, and visible only to the application.
- The key's declaration order is the scan order, and the row cannot change it: "sort
  by a field" is an index (ADR-0005), never a key.
- A side table's faces live in its own ns. Sharing a key never shares storage, and
  the two tables commit independently.

## Related records

ADR-0003 (the assembly point that hides the parameter list), ADR-0011 (full keys in
storage — the address is written as built), ADR-0012 (object model and the
classifier the same-name rule rides on), ADR-0036 (a value too big for the row: its
own ns, the same key), MODELING "A table is a key type plus a row type".