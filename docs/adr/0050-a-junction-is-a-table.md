# ADR-0050: A junction is a table — its own ns, a primary face, a reverse index face

## Status

Superseded by ADR-0051 (2026-11-04): a junction is a collection, so the table this ADR
shapes by hand is a table the model already has. Kept for the analysis and the history.
Supersedes ADR-0015's two-ns residency and ADR-0016's junction segment
(0x5nnn) for junction storage. Implementation lands as one change: the derive, `Junction`,
the storage layout, the export exclusion list, and the modeling guide.

## Context

A junction today is parasitic: `link(a, b)` writes one entry in A's ns and one in B's, both
under a reserved slot of segment 0x5, both with an EMPTY value. ADR-0015 records what that
property costs in the same breath as choosing it — an entry cannot carry relation attributes,
"join time, role, permission", the classic need of a SQL junction table.

The cost is not only the missing payload. A junction with no ns of its own:

- Cannot be enumerated or exported as a table: it exists only as scattered slots inside two
  other collections, so ADR-0045's changelog excludes junctions wholesale.
- Cannot be deleted by endpoint without knowing that endpoint's peer: the two keys are
  different addresses in different namespaces.
- Pushes every application that needs relation data to invent a workaround. The one this ADR
  was written against is two document tables, one per direction — both claiming to be the
  truth, kept consistent by a hand-written double write.

## Decision

**1. One junction type = one namespace**, declared like a document collection. The junction
owns its ns, its primary face and its reverse face. `0x5nnn`, `JUNCTION_ID`, `dir_slot`, and
the two-ns parasitic key builders retire with it; a carriable relation type is a table, so it
gets a table's address.

**2. The primary face (0x00) is the relation row**: `[A·identity][B·identity] -> the relation
document`. This is the only place the relation's data exists. A junction may therefore carry a
payload with any of the machinery a document already has (field types, defaults, codec
placement) — the value is a document, not a byte string.

**3. The reverse face is an index, not a second truth**: `[B·identity][A·identity]` at fixed
face 0x06 (handed over by the graph: an ns is either a graph or a junction, never both), whose
value is a pointer to the primary row. It is a projection: it can be rebuilt by walking the
primary face, and a damaged reverse face is a repair, not a data loss. Fixed faces 0x06/0x07
are reused as they are, without a new allocation.

**4. Direction asymmetry is a SQL secondary index, and is not a defect.** The A direction is a
direct key read; the B direction is an index scan plus a primary fetch per hit. When a hot
query needs the attributes without the hop, covering columns may be added to the reverse face
— a local change that leaves the primary as the single truth.

**5. Cascade is a primitive, not a policy.** `unlink_all(node)` walks the reverse face of every
junction type that names the node's ns and deletes both faces of each row in one batch,
returning the count. Automatic cascade on `purge` stays the caller's decision: it would require
a runtime registry of "which junction types point at this collection", and this model declares
relation types at compile time.

**6. Export follows the two faces separately**: the primary face is a real table and travels to
the lake; the reverse face is derived and stays out, for the same reason the graph's faces stay
out (`Graph::reindex` rebuilds them from the rows). ADR-0045's exclusion list changes from
"junctions" to "a junction's reverse face".

## Alternatives rejected

- **Two document tables, one per direction (the k10r mirror).** Both hold the fact, so
  consistency depends on every writer pairing two writes; and neither can be rebuilt from the
  other. An index face has neither problem.
- **Writing the payload into both parasitic entries.** The same fact stored twice with no
  authority — or stored once, which makes the two entries unequal and every reverse reader
  dependent on direction.
- **Reusing the graph edge for every relation.** Correct for open endpoint types, predicates
  and parallel facts; wrong for a closed pair-typed relation, where a table is the smaller,
  faster statement and the graph's machinery (predicate vocabulary, edge ids, ref decoding) is
  cost with no user.
- **Keeping junctions value-less and modelling attributes beside them.** That is the mirror
  table again, one level down.

## What this does not change

A junction still relates exactly TWO declared types, its identity is still the endpoint pair
(no edge id: one relation per pair per junction type — parallel facts are a graph edge's job),
and a relation is still written and deleted as an explicit event, never as a document field.
