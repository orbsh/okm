# 0042 — Layout Versions Are Stamps, Not Compatibility Promises

> **Languages:** [English](0042-layout-versions-are-stamps.md) (primary) · [中文](0042-layout-versions-are-stamps.zh-CN.md)

**Status:** Accepted (2026-10-27). Applied the same day: k10r's `EntityRow` went
back to `version = 1` with its usage counters at the FRONT, where a reader looks
for them.
**Date:** 2026-10-27

## Context

The payload carries a version byte and the decoder has a compatibility rule: a
payload newer than the reader is refused, an older one is accepted with the fields
it lacks defaulted. That mechanism is right, and it is also a magnet: once a codec
can absorb a shape change, shape changes start to be designed AROUND it.

Two things were written that way before this record.

- The entity row's usage counters were appended at the tail «because appending is
  the legal addition» — a decision about the codec deciding where a field sits.
  A reader looking for a node's counters finds them after the name, in the part of
  the struct that is supposed to be the residue.
- Removing a field (the vector, ADR-0019) shifted every frame, so every existing
  store had to be rebuilt — and that rebuild became a paragraph of reasoning in an
  ADR, as though a memory core that has never held production data owed an upgrade
  path to its own scratch directories.

## Decision

**1. The version byte is a STAMP.** It records the shape the payload was written
in. It is not a promise that the next build can read it.

**2. Fields go where they belong.** Declaration order is chosen for the reader —
identity first, then what a read touches, then the residue — never by what the
codec finds convenient to append.

**3. Compatibility is opt-in, per table, in writing.** A table keeps backward
compatibility only when a record says these payloads must survive a shape change,
and says why. Silence means: rebuild.

**4. A shape change resets the number.** If the field set changes and no record
claims compatibility, the new shape is what the table says it is (usually version
1 again), and a store written by an older build is discarded and regenerated.

**5. Migration, when it is ever needed, is export → import.** The bundle and the
per-table Parquet export already exist, they carry the caller's own rows, and they
are a supported path with an owner. An in-place decoder for every shape ever
written is not.

**6. The decode rule stays as it is.** A newer payload is refused and an older one
defaults what it lacks — that is what makes a stale store fail loudly or land
sane, not what licenses keeping every shape. Defaults are version-migration
semantics, not "fields may be absent".

## Consequences

- **Schemas stay legible.** A field's position says what it is for; nothing is at
  the tail because it arrived late.
- **No ADR per rebuild.** A memory core before real use regenerates its corpus;
  that is a one-line note in a commit, not a migration design.
- **A stale store is never silently reinterpreted.** With the number reset, an old
  payload is either refused by the version check or decodes into nonsense that its
  owner notices — which is why the honest instruction is "delete it", not "read
  it carefully".
- **The cost lands on whoever needs compatibility**, and it lands as a written
  requirement rather than as an accident of the codec.

## Related records

ADR-0019 (the vector's side table — a shape change that was treated as a
migration), ADR-0030 (fixed faces and counting families), okm MODELING §"Payload
versions and field defaults" (the mechanism this record bounds).