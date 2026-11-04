# ADR-0051: A relation table is a collection — Junction retires

## Status

Accepted (2026-11-04). Supersedes ADR-0050, whose own conclusion — a junction is a table —
is taken one step further: it is not a table shaped by junction machinery, it is the table
the model already has. ADR-0015/0016's junction carrier retires with it.

## Context

ADR-0050 gave the junction its own ns, a primary face holding the pair and a reverse face
holding the pair the other way round. The shape was right; the carrier was not. Every
capability that decision had to build already exists for a document collection whose key is
two identities:

| the relation needs | junction (before) | junction (ADR-0050) | a collection |
|---|---|---|---|
| forward scan | one entry per endpoint ns | primary face | first key column, `scan_keys` |
| reverse scan | a second entry per pair | reverse face (ids only) | `#[ok_index]` + **fetch-back of the row** |
| relation attributes | none | needs a payload type parameter | ordinary fields, indexable |
| write both directions | manual `link` pairing | same | `put` writes row + index in ONE batch |
| rebuild derived state | impossible | walk the primary face (unwritten) | `rebuild_faces` |
| export to the lake | excluded (ADR-0045) | primary face only | the row is a row |

`okm-core/tests/relation_table_test.rs` proves the collection column end to end: forward by
prefix, reverse by index with the payload attached, `purge` removing the index entry, and
`rebuild_faces` reproducing it from the rows.

SQL has no junction carrier either. A many-to-many relation there is a table with a two-column
primary key and whatever columns the relation owns; the reverse direction is a secondary
index. That is what this model already calls a collection.

## Decision

**1. A relation is a collection whose key is the pair.** `MembershipKey { channel, member }`;
the first identity is scanned as a key prefix, the row carries `since_ms`, `role`, and any
other column the relation owns.

**2. The reverse direction is a declared index**, `#[ok_index(name { fields(member) })]`,
which the write path maintains in the same batch as the row and `rebuild_faces` can
regenerate. It is not a second copy of the fact.

**3. `Junction`, `JunctionEncode`, `KvJunction`, the `0x5nnn` segment, the direction bit and
the "junction owns an ns" carve-out retire.** No declaration, no descriptor, no faces.

**4. Cascade stays the caller's policy, as everywhere else.** Deleting an endpoint means
scanning the relation table's first key column and purging those rows; there is no
`unlink_all`, because `purge` per row is the same work with the index maintained for free.

**5. A relation table is exported like any table.** ADR-0045's exclusion list loses its
junction entry entirely rather than narrowing it.

## Alternatives rejected

- **Keeping the junction with a payload (ADR-0050 as written).** It reproduces, by hand,
  what `put` + `#[ok_index]` already do automatically, and it keeps a carrier whose derived
  face has no rebuild path.
- **A junction as sugar over a collection** (a macro that generates the key struct and the
  index). The sugar would have to explain when it applies; the underlying shape is two
  declarations a modeler writes anyway, and a second name for one mechanism is the cost
  ADR-0027 §naming already rejected.
- **Two mirrored document tables** (the shape that started this). Still rejected: two truths,
  hand-paired writes, neither rebuildable.

## Consequences

- Relations can carry data and participate in indexes, reduce and export with no new
  mechanism.
- One fewer concept in the model: "many-to-many" stops being a carrier and becomes a key
  layout.
- Migration: declared junctions become collections. Forward scans keep working (prefix); the
  reverse becomes an index; any junction id allocation is freed.
