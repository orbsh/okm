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

## Declaration basics

The four layers land in code as three derive macros. The full declaration
vocabulary:

### Endpoint keys: `KeyEncode`

```rust
use okm::KeyEncode;

/// A user within an org. `org_id` is the organizational prefix;
/// `user_id` is the identity endpoint.
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(1)] // compile-time namespace, folded into the key as big-endian bytes
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(2)]
pub struct SessionKey {
    pub org_id: u32,
    pub session_id: u64,
}
```

Physical layout of `UserKey { org_id: 7, user_id: 101 }`:

```
[ org_id: 4B BE ][ user_id: 8B BE ]   = 12 bytes, zero padding
```

Primary keys are fixed-width: fields are big-endian encoded in declaration
order, `KEY_LEN` locked at compile time. Keys stay fixed-width; variability
is a property of index entries, not keys.

### Edges: `EdgeEncode`

```rust
use okm::EdgeEncode;

/// user → sessions edge.
///
/// Forward direction: a user's identity is (org_id, user_id) → kv_head(org_id, user_id)
/// Reverse direction: a session's identity is the full SessionKey (no kv_head).
///
/// The two directions of one edge use different endpoint identity widths —
/// this is how "the primary key changes with direction" is expressed.
#[derive(EdgeEncode, Clone)]
#[kv_ns(4)]
pub struct UserToSessionEdge {
    #[kv_head(org_id, user_id)]
    pub user_id: UserKey,
    pub session_id: SessionKey,
}
```

`#[kv_head(field, ...)]` declares which fields of the endpoint count as its
*identity* for this edge; omitting it means the full key is the identity.
Names must be a declaration-order prefix of the endpoint's fields
(compile-time generated check). One declaration produces both key families
automatically (direction bit: see "Many-to-many relationships" above).

### Rows and indexes: `RowEncode`

```rust
use okm::RowEncode;

/// A user row hangs off UserKey via #[kv_ref]; payload fields are TLV-encoded.
/// Each #[kv_index] declares an access method over PAYLOAD fields —
/// identity belongs to the key (a surrogate id), business dimensions to the row.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(UserKey)]
#[kv_index(by_reputation { fields(reputation) })]
#[kv_index(by_org { fields(org_id, created_at), includes(bio_len) })]
pub struct User {
    pub org_id: u32,
    pub created_at: u64,
    pub reputation: u32,
    pub bio_len: u16,
}
```

- `#[kv_ref(UserKey)]` — which primary key the row hangs off; identity
  belongs to the key, business dimensions to the row.
- `fields(...)` — payload fields to sort/group by, declaration order,
  first field = the grouping dimension.
- `includes(...)` — covering index: copies payload fields into the entry
  value (see "Covering indexes, with restraint").
- `key(...)` — truncates the primary-key tail carried at the entry end to
  the named subset (default: full). Truncation changes row-uniqueness, not
  grouping: the `fields` prefix drives ordering, the key tail distinguishes
  rows; `key(user_id)` is safe only when the named subset is unique per row —
  otherwise rows overwrite each other's entries.

Physical index entry layout (ADR-0005):

```
[ ns 2B BE ][ indexed fields BE ][ primary key prefix (default: full) ]   value = included fields TLV (empty when no includes)
```

The 2-byte namespace is the only discriminator — no slot byte; declaration
is the registry, no runtime index bookkeeping. An index type's ns is
mechanically derived from declaration order (`table_ns + SLOT`, SLOT =
the index's position among `#[kv_index]` attributes), so **index
declarations are append-only**: add at the tail only — never insert into
or reorder the middle. An insertion shifts the ns of every later index;
entries already on disk stay in the old ns segment, and after the shift
`scan` reads a new prefix and returns empty results (a silent error, not
a slowdown). Removing a declaration merely leaves a harmless ns hole
(same discipline as ns IDs never being reused, ADR-0002). Also note
there is no index backfill: a newly appended index only sees rows
written afterwards; existing rows get no entries — migrate with
double-writes if existing data must be covered.


### Link, unlink, query (`EdgeTable`)

```rust
use okm::EdgeTable;

let store = okm::MockStore::default(); // or FjallStore / SlatedbStore
let mut edges: EdgeTable<_, UserToSessionEdge> = EdgeTable::new(store);

let user = UserKey { org_id: 7, user_id: 101 };
let s1 = SessionKey { org_id: 7, session_id: 1001 };
let s2 = SessionKey { org_id: 7, session_id: 1002 };

edges.link(&user, &s1);   // atomic double write: forward + reverse key
edges.link(&user, &s2);

let sessions = user.get_session(&edges);   // forward: user → [SessionKey]
assert_eq!(sessions, vec![s1.clone(), s2.clone()]);

edges.unlink(&user, &s1); // deletes both directions
```

Physical key layout (forward):

```
[ head 2B: (ns<<1 | dir) BE ][ A·identity ][ B·identity ]
```

`ns = 4`, FWD → head `[0x08, 0x00]`; REV → `[0x08, 0x01]`. The direction bit is niched into the top bit of the namespace field — see [ADR-0001](docs/adr/0001-direction-bit-niche.md).

### Reverse queries and truncated identities

```rust
// Reverse: session → users. A's identity here is truncated (kv_head),
// so raw bytes are returned for a main-table prefix scan.
let raws = edges.reverse_raw(&s1);

// If A had full identity, reverse() decodes back to the type:
// let users: Vec<UserKey> = edges.reverse(&s1);

// PrefixKey marks how many leading bytes are trustworthy.
for pk in edges.reverse_prefix(&s1) {
    // pk.decoded: decoded struct (prefix fields valid)
    // pk.taken:   bytes consumed by the identity prefix
}
```

The derive macro also generates query methods on the endpoint types themselves (`user.get_session(&edges)`), named after the opposite field (`session_id` → `get_session`).

### Rows at runtime (`Table`)

Declaring rows and indexes (`RowEncode` + `#[kv_index]`) is covered in the
[Modeling Guide](docs/MODELING.md), "Declaration basics". The index declaration's derivative is generated at the expansion point
(this file): `kv_index(by_org ...)` generates the index type
`__OkmIndex_User_by_org` (mechanical concatenation, no case conversion);
alias it with `use` and it serves as the generic parameter. `Row::table`
builds the assembly point without repeating the key type at the call site:

```rust
use okm::{MockStore, Row};
use __OkmIndex_User_by_org as ByOrg; // index type: derived from kv_index(by_org)

let mut t = <User as Row>::table(MockStore::default(), 9);

t.put(&user, &User { org_id: 7, created_at: 30, reputation: 100, bio_len: 2 });

// Leftmost-prefix scan on any access method, with fetch-back:
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes());
for (key, row) in rows {
    // key: decoded UserKey, row: Some(decoded User) when the payload exists
}

t.delete(&user); // removes the primary key + all declared index entries
```

### Schema stability tests

Lock the physical bytes with hard-coded hex — any layout drift fails CI:

```rust
let fk = edge.forward_key();
assert_eq!(&fk[..2], &[0, 8]); // ns=4, FWD — direction bit in the low bit of the BE pair
assert_eq!(&fk[2..6], &7u32.to_be_bytes());
// ... full layout assertions in okm/tests/integration.rs
```

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
