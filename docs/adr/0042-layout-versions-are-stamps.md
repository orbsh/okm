# 0042 — Layout Versions Are Stamps, Not Compatibility Promises

> **Languages:** [English](0042-layout-versions-are-stamps.md) (primary) · [中文](0042-layout-versions-are-stamps.zh-CN.md)

**Status:** Accepted (2026-10-27). Applied the same day: k10r's `EntityRow` went
back to `version = 1` with its usage counters at the FRONT, where a reader looks
for them. Refined 2026-10-28: the number may simply increment (resetting was never
the point), and the record now says what would actually earn compatibility.
**Date:** 2026-10-27, refined 2026-10-28; amended 2026-11-10 — the stamp moved out
of the payload into the SEGMENT header, and with it went the per-row decode rule

## Context

The payload used to carry a version byte, with a decoder rule beside it: a payload
newer than the reader refused, an older one accepted with the fields it lacks
defaulted. That mechanism is also a magnet: once a codec can absorb a shape change,
shape changes start to be designed AROUND it. The amendment below removes the byte and
keeps the thesis — the number is an identity, and it now lives where the identity can be
checked without decoding anything.

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

**1. The layout number is a STAMP, and it is the SEGMENT's.** It records which shape
the bytes were written in. It is not a promise that another build can read them. The
payload carries no header at all — `[hot segment][cold TLV]`, the boundary being the
declaration's `HOT_WIDTH` — so a row repeats nothing its reader already knows, and an
empty document is zero bytes. The number is stated twice: in the segment header
(`layout_version`) and in the declaration (`#[ok_layout(version = N)]`, default 1). The
header's number decides which declaration decodes that segment — the sidecar named
`schema-v<N>.json` beside it — so a directory that carries several generations is the
ordinary state of a log that has outlived a build, and a build opens it whatever its newest
segment says. What is refused is a segment whose generation has no sidecar: the reader stops
and names the number rather than borrowing a neighbour's declaration. There is no per-row
comparison, because a row is never read without the log it came from, and no directory-level
verdict, because a generation stamps BYTES and not a path.

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

**5. Migration is REPLAY, not a dual reader.** The build replacing the old one reads the
change log and writes the rows through its own write path, decoding each segment under the
declaration that produced it and re-encoding it under its own (ADR-0056 §5). Nothing keeps
a decoder for another shape alive: the old declaration is a sidecar file, which is data, not
a second reader in the binary. An in-place upgrade-on-read path (old records staying in the
old format, a build absorbing a shape it does not declare) is not the design — and neither
is routing an upgrade through Parquet, which cannot express a removal.

**6. What a decoder still absorbs is absence, and only in the cold half.** A cold TLV
frame that is not there is a field that was never written, and it takes its declared
default (`#[ok_default]`, else `T::default()`) — that is a property of the frame format,
not of a generation. A **hot** append moves the boundary and shifts every cold frame
after it, so it is a shape change: a new number, a new generation. Defaults are not
"fields may be absent anywhere", and they are not a migration strategy.

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
- **A stale store is never silently reinterpreted.** The number differs, so the log is
  refused at open rather than decoded — the honest instruction stays "export it with the
  build that wrote it, or delete it".
- **An in-place binary swap is free exactly when the number did not change**, which is
  the common upgrade: nothing moves, the downtime is a restart. A changed number makes
  the swap a rebuild instead — beside the serving store (blue-green, second copy of the
  data for the length of the cutover, cut over table by table so the peak is "everything
  + the largest table") or in place with the service down.
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