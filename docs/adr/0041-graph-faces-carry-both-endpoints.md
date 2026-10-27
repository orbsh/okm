# 0041 — The Graph's Indexes Carry Both Endpoints

> **Languages:** [English](0041-graph-faces-carry-both-endpoints.md) (primary) · [中文](0041-graph-faces-carry-both-endpoints.zh-CN.md)

**Status:** Accepted and LANDED (2026-10-27). Slots renumbered with no gaps,
`Graph::reindex` added for stores written before it.
**Date:** 2026-10-27

## Context

Five fixed faces served the edge collection:

```text
0x06  predicate index   [predicate][edge_id]
0x07  out-edges         [src][edge_id]
0x08  in-edges          [dst][edge_id]
0x09  predicate+out     [predicate][src][edge_id]
0x0A  predicate+in      [predicate][dst][edge_id]
```

Every one of them names ONE endpoint, so the pair is not addressable. "Is this
assertion already in the graph" — the question a write path asks before it writes,
and the question a re-reading pass asks every time it re-reads a span — could only
be answered by scanning the source's entire out-degree under that predicate and
filtering the destination in memory. On a hub (a member node, a task node) that is
the hottest and worst-shaped read in the store.

Two further things were true at the same time: `0x06` was a *denormalized* copy —
`[predicate][edge_id]` is exactly what a `[predicate]` prefix of `0x09` yields,
with the id in the same trailing position — and the vector index snapshot sat at
`0x0B`, leaving a hole at `0x0A`'s successor once the faces were re-thought.

## Decision

**1. Every face carries both endpoints, in traversal order.**

```text
0x06  out-edges         [src][dst][edge_id]
0x07  in-edges          [dst][src][edge_id]
0x08  predicate+out     [predicate][src][dst][edge_id]
0x09  predicate+in      [predicate][dst][src][edge_id]
```

Adjacency is preserved because the traversed endpoint still leads; the pair is
additive. `(src, dst)` is addressable on 0x06 and `(predicate, src, dst)` on 0x08 —
the latter IS the assertion identity, as one seek.

**2. The predicate index retires.** Scanning 0x08 by its bare `[predicate]` prefix
returns the same edge set 0x06 returned. It is a prefix scan, not an intersection:
deriving "all edges of P" by intersecting the predicate's edges with a node's edges
would re-do the work the index exists to skip, and `edges_of_predicate` is a real
query (`MATCH ()-[r:P]->()`), not a convenience.

The cost is bytes, not entries: each suffix now carries two endpoints where the old
face carried the id alone. One fewer face also means one fewer put and one fewer
delete per `link`/`unlink`.

**3. The suffix rule follows the prefix.** After a single node the entry is
`other endpoint + edge_id`, and the id is the trailing eight bytes — the `NodeRef`
is self-delimiting, so it cannot be confused with them. After a pair (or a predicate
and a pair) the prefix consumed both endpoints and the suffix IS the id, so the
fixed-width convention survives exactly where it is load-bearing.

**4. There is no `edge → endpoints` face, and that is deliberate.** The missing
capability in the old layout was *read*, not index: the edge's primary row already
carries both endpoints (`get_edge` decodes `src` and `dst` before anything else, and
`check_members` is the check that both belong to this graph). A face keyed
`[edge][head][tail]` would store the row's own data a second time, and its key order
would destroy adjacency — you cannot scan by head when the edge id leads. What the
store owed was a verb (`edge(id).src/.dst`), not a face.

**5. Slots are compacted with no hole.** The graph holds 0x06–0x09, and the vector
index snapshot moved to 0x0A. The snapshot is a droppable cache, so its move costs
rebuild time and nothing else; a stale blob under the old number cannot be mistaken
for a new one either, because the snapshot header's magic and version are checked
before anything is decoded.

**6. An existing store is reindexed, not read.** The keys changed AND the numbers
changed: the retired predicate index sat exactly where out-edges live now, so
entries written by an older build would be read as something they are not. That is
why the wipe is mandatory rather than optional, and why it is a method on the graph
(`reindex`) rather than a note in a migration guide: it scans the four faces'
prefixes, deletes them, then re-derives every entry from the primary rows. The
primary table — the only thing that cannot be rebuilt — is never touched.

## Consequences

- **The assertion identity is one seek** (`typed_out_to`), whatever the source's
  out-degree. k10r's duplicate check becomes an exact lookup; before it, every write
  paid for a scan and a filter.
- **A re-reading pass is harmless.** A pass that reads an overlapping span writes
  the same facts again, and they land on the edges that already stand.
- **A `link` writes four face entries, not five**, and an `unlink` deletes four.
- **`edges_of_predicate` pays bytes.** Same entries, longer keys; the predicate scan
  reads two endpoints per entry it skips past.
- **Two suffix rules now exist, and mixing them up fails closed.** A reader that
  expects a bare id in a single-node scan finds a longer suffix and returns nothing
  rather than garbage — the same direction the fixed-width decoder always failed in,
  and the reason the pairing helper and the plain helper are separate functions.
- **`reindex` is a write path with no clock and no incremental form.** It is a
  one-shot upgrade for a store smaller than the memory it would need to be smarter
  about; a store too large for it needs a schema version and a background walk,
  which nobody needs yet.

## Related records

ADR-0017 (the graph edge), ADR-0030 (fixed faces and counting families), ADR-0016
(the slot table this replaced).