# Modeling Guide

How to map a domain onto OKM keyspaces: four layers per entity, access
methods mandatory, covering indexes restrained, composite keys bounded.
This guide is normative for application schema; for mechanism details see
the ADRs and the [README](../README.md).

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## The four layers

Every entity is modeled in four layers: **namespace → primary key →
primary sort field → access methods**.

1. **Namespace (`ns`)** — which keyspace the entity lives in. One entity
   class = one ns. The 2-byte ns already discriminates every entry; do
   not encode entity type into keys.
2. **Primary key** — identity, and nothing else. Proxy keys (auto-increm
   id, UUID) are the default: attributes are payload, not identity.
3. **Primary sort field** — the third segment of the key
   (`[ns][pkey...][order]`). Placed at the key tail, it keeps entries
   under the same pkey physically adjacent and ordered: a prefix scan
   *is* the timeline / range read. Other sort dimensions stay out of the
   key and surface through `#[kv_index]`. How physical decisions trade
   against indirect costs: see "Design constraints on performance".
4. **Access methods** — declared `#[kv_index]` entries. Each one is a
   standing answer to "how is this entity queried?".

If you cannot name the access methods, the model is not finished — the
questions you cannot name become full scans.

## One-to-many relationships

Access methods are not only about ordering: **one-to-many relationships
also land as prefix grouping**. A layout like
`[tenant_id][org_id][user_id]` takes all users of one org by prefix
scan — this is the physical form of the relational "foreign key + list".

In OKM it appears as an independent ns plus a secondary index. The index
entry layout is `[ns+slot][fields segment][primary-key prefix]`:
`fields(org_id)` is the grouping/sort segment in the middle of the entry
(payload fields encoded in declaration order, derive-generated,
compile-time layout locked) and provides the grouping prefix;
`includes(...)` is the entry value, copying frequently used fields so
the scan needs no fetch-back. Note this is independent of the primary
key layout: the grouping dimension lives in the index entry — composable,
retirable, with no "nowhere to decode the suffix" problem. You could
also store user data directly inside the list entries (the nested form;
"denormalized storage" in SQL terms). Storing together is efficient but
locks the user to the org — a job change moves data, the same trap as
primary-key composition.

Against SQL normalization: the independent ns *is* the normalized form —
org attributes like parent_id and org_name are stored once on the org
row, not on user rows. Storing together (denormalized) is efficient but
couples; apart (normalized) pays one indirection but keeps identity
singular.

## Many-to-many relationships: edges

The access methods defined by `#[kv_index]` are **table-local** — their
data source is the row's own payload, kept in sync automatically by
`put`/`delete`. Cross-table relationships (one-to-many, many-to-many)
are facts between two independent entities; a payload index cannot
reach them, so they are expressed as **edges**:

```text
#[derive(EdgeEncode)]
#[kv_ns(4)]
struct OrgUserEdge {
    #[kv_head(tenant_id, org_id)]   // A-side identity truncated to (tenant_id, org_id)
    pub org: OrgKey,                // B side, no annotation = full UserKey
    pub user: UserKey,
}
```

Edges materialize as explicit forward+reverse keys (ADR-0001 direction
bit): the top bit of the ns is the direction. The FWD prefix
`[ns:4][tenant][org]` fetches all members of an org in one scan (the
list side); the REV prefix fetches all orgs a user belongs to. A job
change = adding/removing one edge; identity never moves.
`#[kv_head(...)]` declares which identity fields the endpoint is
truncated to on that direction (absent = full identity), letting each
endpoint use a shorter prefix — edge key length and grouping granularity
are trimmed per direction, which is how "the primary key varies by
direction" is expressed. `kv_head` selects a subset of *declared
identity fields*, not free bytes; its decidability follows the same
principle as the index tail.

Edges and indexes are isomorphic at the mechanism level (both are
"secondary key layouts pointing at identity") but their roles cannot be
swapped:

- **Data source**: an index's data source is the row's own payload —
  the user never writes entries by hand. An edge's data source is the
  relationship between two entities — a business fact in its own right,
  requiring explicit double-write and double-delete.
- **Endpoints**: an index's tail points at its own table's primary key;
  an edge points at another table's entity (two endpoints, direction bit
  separating forward and reverse).

**Bidirectionality is an obligation, not an option.** A REV entry costs
one extra key of storage (LSM sequential append — the cheapest write
there is); skipping it buys: reverse lookups degenerate into full scans
of the FWD segment (violating access-method discipline), or a later
backfill migration (orders of magnitude more expensive). The contrast
with indexes makes this sharper: an index has only one direction,
because the reverse lookup goes through the primary-key `get`; both
endpoints of an edge are secondary vantage points — neither holds the
primary key — so both directions need an entry. Bidirectionality also
makes unbinding O(1): both keys' identities are in hand, so it is an
exact `delete`, not scan-then-delete. The real restraint is not "do we
need the REV direction" (no choice there) but **whether this edge
should exist at all**: a relationship with no reverse-lookup need and a
small cardinality (a config-style one-to-one) can live in a payload
field, with no edge declared; add one later when needed — edge
double-writes sync automatically, no backfill.

One sentence: **an index is a derived view of one row; an edge is
first-class relationship data**. The convenience of `#[kv_index]`
(declare-to-register, no manual ns numbering — ADR-0005's motivation
was precisely eliminating per-index manual numbering and hole
bookkeeping) is a secondary bonus, not the dividing line; the dividing
line is the data source.

## Access methods are mandatory

Queries go through `get` (primary key) or `scan::<I>` (a named access
method). There is no general-purpose full-scan query surface.

The only raw scans are `scan_rows_raw` / `scan_keys` — maintenance
surfaces for the snapshot exporter and the Arrow bridge, not application
query paths. If application code reaches for them, that is a missing
access-method declaration, not a shortcut.

## Key naming: plain prefixes + composable access methods

A key has three segments: `[ns][pkey...][order]`. The sort field sits at
the key tail (a big-endian u64 timestamp is byte-order = time order, a
natural timeline): entries under the same pkey are physically adjacent
and ordered, and a prefix scan directly *is* the timeline / range read —
restoring a session, fetching the latest N, all one `scan`. Putting the
primary sort field into the key is a physical decision; other sort
dimensions stay in the payload and surface through `#[kv_index]`.

Fancy prefix schemes are not recommended (path-style, semantically
segmented prefixes) — the longer the prefix, the harder to manage; every
prefix is an encoding convention that must be maintained. Prefer
declaring more access methods: each `#[kv_index]` is an independent,
composable, extensible read path — declared to register, one line
deleted to retire.

**Consider a sort key only when the query result is plural.** The value
of the `[order]` segment is gathering "a group of related entries" into
a physically adjacent, ordered span that one prefix scan retrieves
whole; for a single-result (point) query use `get` — hoarding sort
segments in the key is meaningless.

Sort segments in composite keys deserve their own analysis. Start from
the physical property of sort keys: **a sort key like time is highly
selective and has no grouping power** — its values differ on almost
every entry, so anything placed after it lands in groups of one, and
intra-group ordering is impossible. Therefore appending fields after a
highly selective sort key is meaningless: it can only contribute scan
cost, never a read path.

Anti-pattern: `[session_id][user_id][time]` (session messages).

- user_id sits before time, so entries are ordered "grouped by user,
  then by time within the group" — queries that group a session *by
  user* generally do not exist, and the user_id segment yields no read
  path.
- The cost is certain: even the most ordinary session replay (fetching
  all messages) does not come back in time order — the read side must
  re-sort; fetching the latest N means scanning the whole group and
  sorting.
- If occasional per-user filtering is truly needed, a full scan with a
  filter suffices — not worth changing the key layout for.

The correct form is `[ns][session_id][time]`: the pkey carries exactly
one sort key, and session replay, pagination, and range reads are all
one prefix scan away.

Generalized: **a trailing remainder after `[pkey][sort field]` is also
an anti-pattern.** Stuffing a "also query/group by X" dimension into the
key tail is, in essence, hand-maintaining an access method inside the
key. Hand-maintained key prefixes can indeed buy performance in specific
scenarios, but:

- **Scenario-dependent**: the key layout freezes one read path. As
  business complexity grows, most tables need multiple access methods;
  optimizing for just one is of little value.
- **Business-logic-dependent**: whether the optimization holds depends
  on the business happening to support that read pattern — not a
  modeling technique.
- **Poor cost/benefit**: high development cost, and the performance
  gain is small (quantified in the next section, "Design constraints on
  performance").
- **Nowhere to decode the suffix**: a custom segment at the key tail
  needs a matching decode method, which OKM explicitly does not
  provide — the `KeyEncode` derive decodes only the declared full key
  length (`KEY_LEN` locked at compile time), and `PrefixKey` decodes
  only the truncated identity prefix (bytes beyond the prefix are
  garbage by contract). Hand-writing offset arithmetic to decode a
  suffix is complex and error-prone, precisely losing OKM's point:
  zero-cost encode/decode with compile-time locked layouts.

These dimensions all belong to access methods / secondary indexes:
`#[kv_index]` registers on declaration — composable, extensible, added
as the business grows — without touching the primary key layout.

## Two kinds of tails: index tails are decodable, primary-key tails are not

"Carrying data in the key tail" comes in two forms with completely
different decidability, and they are easy to confuse:

```text
index entry     [ns+slot][fields segment][primary-key prefix]  ← tail always decodable
primary entry   [ns][pkey...][custom tail?]                    ← custom tail undecodable
```

**Index tails are decodable**: the tail segment of an index entry key is
the primary-key encoding — a struct declared via `#[kv_ref]`, whose
exact layout the `KeyEncode` derive knows; cut it off the end of the
entry key and decode it back into the primary key (`PrefixKey`; the
ADR-0005 contract: "the tail is always decodable from the last bytes of
the entry key"). The `fields(...)` segment is the same: payload fields
encoded in declaration order, all encode/decode generated by the derive
with compile-time locked layouts.

**Primary-key custom tails are not decodable**: the primary key's layout
is whatever the user's key struct declares; `decode` decodes only the
declared full length. Stuff a "remainder" into the key tail that the key
struct does not contain, and no generated code knows its offset or
width — the `PrefixKey` contract even states that bytes beyond the
truncated prefix are garbage.

The root in one sentence: **a tail is decodable only if its layout is
inside the declaration system.** The index tail (primary-key encoding,
`fields` segment) is inside the declaration system, hence decodable; a
primary-key "remainder" is outside it, hence undecodable — and should
not be stuffed in; the need belongs to `fields(...)`.

Terminology: **"access method" is the modeling-layer word** — a declared
read path; this guide uses it. **"Secondary index" is the
mechanism-layer word** — the index entry layout generated by
`#[kv_index]`; the ADRs and README use it. `#[kv_index]` is the mount
point for both. This document addresses modeling, so the body uses
"access method" throughout and says "secondary index" only when
touching mechanism layout.

## Design constraints on performance

The constraints above are not free, but the cost is bounded and the
structural benefit outweighs it:

**Reading from the key vs reading via an access method.** A remainder
burned into the key rides back for free with every entry (tail bytes in
hand); via an access method, that dimension comes back through one
indirection — a `scan` fetch-back pays one primary-table point read
plus one `decode_payload` per entry; `scan_covered` skips the point
read but still pays one entry-value parse. Per entry, yes, one extra
parse.

**The per-hit cost is small, and amortized.** That indirection is a
routine LSM point get / one TLV parse — not hot-path scale; what it
buys is a key layout that never deforms for every "also query by X"
dimension. As the business grows you add `#[kv_index]` instead of
redesigning keys.

**Bulk reads actually come out ahead.** Fetch-back looks like random
point reads, but both streams are already ordered: index entries are
clustered and ordered by `[pkey][order]`, and the primary table by
`[pkey]` — fetch-back degenerates naturally into a two-pointer merge
(merge-join style), sequential I/O, far cheaper than per-entry random
reads. Hand-optimized key layouts have no extra card to play in the
bulk case.

Conclusion: the indirect cost of an access method is one amortizable
parse plus one mergeable ordered read; bounded, controlled runtime
overhead in exchange for unbounded key-layout complexity — a trade that
only pays more as business complexity grows.

## Covering indexes, with restraint

`includes(...)` copies payload fields into the index entry so
`scan_covered` needs no fetch-back. A real win for high-fanout read
paths — but every put/delete pays a permanent write cost.

Discipline:

- **Default to `fields` only.** Model with sort fields first.
- Add `includes` only after measurements show the fetch-back path is
  hot. It is an optimization for a proven hot path, not a default
  modeling move.

## The boundary of primary-key composition

Primary keys may be composite (see the four layers:
`[ns][pkey...][order]`), but the composed components must be part of
the identity structure — and identity structure is answered by the
business, not by modeling technique.

The canonical case is SaaS multi-tenancy: tenant → org → user, three
levels. tenant_id and org_id are bound (an org belongs to exactly one
tenant); user_id is not bound to an org — a user can change jobs,
changing org while identity stays. In concrete layout terms (square
brackets = key segments, braces = row fields; a field that is both key
and row is written in both places):

```text
Org (with hierarchy; parent_id expresses the tree)
  [ns:org][tenant_id][org_id]{name}{parent_id}

User
  [ns:user][user_id]{name}...{org_id}
```

- The org primary key is the composite `[tenant_id][org_id]`:
  tenant_id draws the physical boundary of data ownership, org_id is
  identity within the tenant — both are identity structure.
- Trees do not cross tenants: the parent referenced by parent_id is in
  the same tenant as the child, so `{parent_id}` on the org row is
  enough to express the hierarchy — no separate adjacency-table entity
  is needed. Level-by-level expansion (BFS/DFS) within the
  `[ns:org][tenant_id]` prefix is just layered `get` by parent_id; one
  access method covers it.
- The user primary key is only `[user_id]`. org_id is a row field —
  compose `(org_id, user_id)` into the primary key and a job change
  means a key change: one physical person split into several
  identities.

Reads by org go through an index: `#[kv_index(by_org {
fields(org_id, ...) })]` answers org-scoped reads with one prefix scan
while identity stays singular.

Ownership questions like "does user-generated data belong to the
company or the person" follow the same logic — they are business
questions, and when the answer changes (data migration, ownership
transfer) identity should not. This is the row model's (ADR-0006)
payload-not-identity rule, restated as modeling discipline.

## Evolution

New modeling principles are appended to this document as sections;
mechanism decisions go into ADRs and are referenced, not duplicated.
