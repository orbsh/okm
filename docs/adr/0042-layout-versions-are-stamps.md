# 0042 — Layout Versions Are Stamps, Not Compatibility Promises

> **Languages:** [English](0042-layout-versions-are-stamps.md) (primary) · [中文](0042-layout-versions-are-stamps.zh-CN.md)

**Status:** Accepted (2026-10-27). Applied the same day: k10r's `EntityRow` went
back to `version = 1` with its usage counters at the FRONT, where a reader looks
for them. Refined 2026-10-28: the number may simply increment (resetting was never
the point), and the record now says what would actually earn compatibility.
**Date:** 2026-10-27, refined 2026-10-28

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

**1. The version byte is a STAMP.** It records which shape a payload was written
in. It is not a promise that another build can read it.

**2. The number may simply go up, and that is the ordinary style.** A shape change
gives the table a new number — incrementing is the default, and resetting to 1 is
equally legitimate (a table with no history worth distinguishing, a corpus that
will be regenerated anyway). The number has exactly one duty: two incompatible
shapes must be nameably different. It is not a ladder, and version N+1 is under no
obligation to read version N's bytes.

**3. Fields go where they belong.** Declaration order is chosen for the reader —
identity first, then what a read touches, then the residue — never by what the
codec finds convenient to append.

**4. No table owes a compatibility reader.** Reading an older shape is a per-table,
WRITTEN decision (a record that says these payloads must survive a shape change,
and says why). Silence means the store is regenerated or carried across by
migration — never that the new build keeps a decoder for the old shape.

**5. Migration is export → import, not a dual reader.** The build that wrote the
data exports the rows it can read; the build replacing it imports them under its
own layout. The bundle and the per-table Parquet export already exist, carry the
caller's own rows, and have an owner. An in-place upgrade-on-read path (old
records staying in the old format) is not the design.

**6. The decode rule stays as it is — a failure mode, not a migration strategy.** A
newer payload is refused and an older one defaults what it lacks, which is what
makes a stale store fail loudly or land sane when it is opened at all. Defaults are
not "fields may be absent".

**7. What would actually earn compatibility — and how a production upgrade really
happens.** The scenario that pays for it: production data that cannot be lost and is
large enough that an export/import window is unacceptable. A tolerant decoder is
still not the answer. The answer is a BLUE/GREEN move: the new build runs as a
second process on its own store, the old instance exports, the new one imports — the
backfill can take hours or days, and both sides stay live meanwhile — then a short
stop for the incremental catch-up import, then traffic switches to the new instance.
Where the volume is the problem, the cold side of that move is a lakehouse /
object-store export (or an engine that owns its own on-disk format, slatedb-class),
so the backfill is a bulk load rather than a re-import through the write path.

Two builds never share one store — a KV store has a single writer — so even an
upgrade that changes no byte format needs a stop-and-swap. That is a structural
disadvantage next to SQL. In practice it is smaller than it looks: a second or two of
downtime sits inside the latency users already blame on the network; a gateway retry
policy keeps the request from ever surfacing as a failure; traffic prediction picks an
idle window for the cutover — even military equipment has maintenance windows. SQL's
story is better on paper, and honestly: DDL lets the schema change online, so a
seamless upgrade is theoretically possible. The difficulty is in the APPLICATION, not
the engine — multi-environment testing, a snapshot before the release, a rollback
plan — so for development SQL is clearly ahead on this axis, while in production the
advantage is much smaller than it reads. Looking seamless is the bar.

## Consequences

- **Schemas stay legible.** A field's position says what it is for; nothing is at
  the tail because it arrived late.
- **No ADR per rebuild.** A memory core before real use regenerates its corpus;
  that is a one-line note in a commit, not a migration design.
- **A stale store is never silently reinterpreted.** The number differs, so an old
  payload is either refused by the version check or lands on whatever the decoder
  defaults and its owner notices — the honest instruction stays "export it with the
  build that wrote it, or delete it".
- **The cost lands on whoever needs compatibility**, and it lands as a written
  requirement paid in export/import tooling or in an engine's own format, never as
  an accident of the codec.
- **The upgrade story is a deployment shape, not a codec feature.** Blue/green with a
  short cutover stop; the standing seconds of downtime are the price of a
  single-writer store, and they are scheduled, retried at the gateway and absorbed by
  users who never see them — not designed around in the payload.

## Related records

ADR-0019 (the vector's side table — a shape change that was treated as a
migration), ADR-0030 (fixed faces and counting families), okm MODELING §"Payload
versions and field defaults" (the mechanism this record bounds).