# Query Recipes: okm-query Combinators and Their SQL Shapes

A recipe collection for okm-query: how the three combinators
(merge_join / group_by / walk) compose into shapes familiar from SQL,
plus two boundary records — the physical/in-memory split of WHERE
(prefix, range, filter), and
the three landings of grouping. Mechanism details live in internals
([index-mechanism](internals/index-mechanism.zh-CN.md)); modeling
discipline lives in the [modeling guide](MODELING.md).

## Core stance

okm-core's obligation ends at "one ordered stream per access method";
composition (join, grouping, graph walks) is algorithm-layer work, all
in okm-query, zero core changes — every input is an existing `scan` /
`scan_index` output. No query engine, no planner: a recipe is Rust
code, declaration is execution.

```text
SQL concept      okm landing
────────────────────────────────────────────
WHERE (physical) index declaration + prefix/range scan (free — see the record)
WHERE (memory)   .filter() (stdlib, no wrapper)
GROUP BY         group_by (index sort does the grouping) or reduce (compile time)
JOIN             merge_join (sorts its own inputs)
Multi-rollup     prefix scans narrowing the group segment level by level
Graph walk       walk (one hop = one prefix scan)
```

## The GROUP BY recipe

### Index-sort grouping (read time)

Put the group field first in `fields(...)`; the sort IS the grouping:

```rust
#[ok_index(by_org { fields(org_id, created_at) })]
struct User { org_id: u32, created_at: u64, ... }
```

```rust
// Count per org: one full prefix scan over the entries, group_by folds
// in a single pass.
let rows = t.scan::<User_ByOrg>(&[]);
let counts = okm_query::group_by(
    rows.iter().filter_map(|(_, r)| r.as_ref()),
    |u| u.org_id,
    || 0u64,
    |acc, _| acc + 1,
);
```

The input is ordered (every scan is), so group_by is single-pass —
the grouping cost equals the index sort cost, already paid at write
time.

### Multi-level rollup (one stream, narrowing prefixes)

The group segment is bytes encoded into the entry key; narrowing the
prefix level by level IS the rollup, no re-scan needed:

```text
index over fields(org_id, dept_id, created_at):

scan(&[org])                    → everything in the org (rollup level 1)
scan(&[org, dept])              → org × dept (level 2)
scan(&[org, dept, day])         → org × dept × day (detail)
```

Each level is one O(hits) prefix scan; rollup levels are chosen on
demand — one physical layout declaration covers all of them.

### Three landings of grouping (same encoders)

| Landing | When | Semantics | Fits |
|---|---|---|---|
| `#[ok_reduce]` | compile time | reversible aggregate, maintained on the write path, resident | fixed groups, read-heavy |
| `group_by` (index sort) | read time | single-pass fold, varies per query | ad-hoc grouping, multi-dimension |
| reduce GROUP vs index-first-field | — | — | the same field encoders, different landing time |

Criterion: fixed group dimensions with hot reads → reduce (pay at
write, read free); varying dimensions → index sort + group_by (pay at
read, one layout covers many dimensions). Not exclusive: one index can
serve group_by and the timeline scan at once.

## The JOIN recipe: merge_join and difference

Join on a column both sides index. A scan hands back the field segment it sorted
by, so the join key is in hand with no fetch-back and no `includes`:

```rust
let left = orders.scan::<Order_ByUser>(&[])?          // ordered by user, then by key
    .into_iter()
    .map(|(f, k, _)| (f.user_id, k.decoded));
let right = users.scan::<User_ById>(&[])?             // ordered by id
    .into_iter()
    .map(|(f, k, _)| (f.id, k.decoded));

let pairs = okm_query::merge_join(left, right);        // (OrderKey, UserKey)
```

Two faces of the SAME table join on the primary key instead (two posting lists,
say): there the merge key is the key, because a token's prefix fixes the data
segment and the entries under it run in key order.

**What is ordered, and what costs a sort.** An index scan is ordered by the entry
tuple `(fields…, key)`, and any PREFIX of that tuple is still ordered. So a join
on the leading index field needs no sort; a join on a column that is not a prefix
— the primary key with a field in front of it, a range probe, a multi-value
index's keys — needs one, and `merge_join` takes it. Which shapes come out ordered
is a performance question, never a correctness one: the operator sorts anything.

No hash join, deliberately: a sort-merge join needs no hashing of the key type
and holds `O(n)` rather than materializing a build side.

**Outer joins are a difference, not a third operator.** `difference(a, b)` answers
the left entries whose key has no match on the right, so both sides of an outer
join fall out of the same two streams:

```rust
let left_only  = okm_query::difference(left.clone(), right.clone());
let right_only = okm_query::difference(right, left);
```

**More than two streams: fold.** An N-way join is `merge_join` applied pairwise —
for a pure intersection map each side to `(key, ())` first, since a join of joins
needs the key carried in the value. For exclusives, intersect once and subtract per
side: `difference(a, common)`, `difference(b, common)`, `difference(c, common)` —
each answer is that side's "more than the intersection".

Both operators sort their own inputs and return `Vec`, so a chain materializes at
every step: `LIMIT` stops nothing early. Feed a cheap, highly selective scan first.

## The graph recipe: walk

One hop = one prefix scan per direction (the edge layer guarantees
both exist):

```rust
// Friends of friends: two hops, one prefix scan per edge type per hop
// per direction.
let found = okm_query::walk(&[&friend_edge], &store, &start_bytes, 2);
```

The cost model is identical to the edge layer's forward/reverse; the
only added structure is the frontier and the visited set.

## Boundary record: the forms of WHERE

- **prefix = physical WHERE (equality).** The byte range a prefix scan
  hits IS the storage-layer filter, free (non-hits are never read).
  Selectivity belongs in key layout — "how is this entity queried"
  decides what leads `fields`. Pushing a key-qualifying predicate into
  `.filter()` abandons the storage layer's selectivity: every query
  pays decode cost for rows it throws away.
- **primary-key prefix = the same WHERE with no access method.** A
  composite key's leading segments group rows by construction, so
  `scan_keys` takes a prefix probe and answers "every child of this
  parent" with one prefix scan over the primary table —
  `scan_keys(&DeviceKey::prefix().org(7).finish())`, the probe naming
  fields in declaration order. A full scan is the EMPTY probe
  (`PrefixProbe::empty()`), not a second door. It is lazy: `.take(n)` is
  a LIMIT that pays only the reads performed, `.rev()` walks the window
  backwards, and the answers are ROW keys (`fetch` is the materializing
  door; `scan_document_keys` is the eager two-slot mode). Reach for it
  when the grouping is already the key; if the dimension is not a
  leading segment, it is an access-method question, not a probe.
- **range = the interval form of physical WHERE (ADR-0020).** A
  predicate on the LEADING index field with byte order == value order
  is a key interval — `1 < a < 100` reads only the rows inside
  `[1, 100)`, via `scan_range` (buffered) or `scan_range_iter` (lazy:
  LIMIT / first-match stops pulling; `.rev()` reads backwards, free).
  Bounds are the encoded bytes of the leading field, caller-owned.
  The difference from a prefix is not cost — both read only hits — but
  SHAPE: prefix = equality on a leading segment, range = comparison on
  the leading field. Neither needs `.filter()`.
- **exact = full-field equality (`scan_exact`).** The probe must name
  EVERY field of the data segment: only an entry the probe consumes
  exactly is a hit, which excludes both directions at once (a longer
  stored value, and a probe completed by the primary-key tail) — exact
  matching with no fetch-back validation. Naming only leading fields is
  grouping, which is prefix's job.
- **`.filter()` = in-memory WHERE.** _stdlib suffices, no wrapper_:
  `.filter()` straight over the scan's output. Reserved for predicates
  that cannot enter a key (non-prefix dimensions, cross-field
  conditions, computed predicates).

One-line criterion: **first ask whether the predicate can become a
prefix (equality) or a range on the leading field (comparison) of some
access method; if yes → change the index declaration / bounds, if no →
filter**. A prefix and a range are free; a filter pays per row.
