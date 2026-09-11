# Query Recipes: okm-query Combinators and Their SQL Shapes

A recipe collection for okm-query: how the three combinators
(merge_join / group_by / walk) compose into shapes familiar from SQL,
plus two boundary records — the physical/in-memory split of WHERE, and
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
WHERE (physical) index declaration + prefix scan (free — see the record)
WHERE (memory)   .filter() (stdlib, no wrapper)
GROUP BY         group_by (index sort does the grouping) or reduce (compile time)
JOIN             merge_join (both sides key-ordered streams)
Multi-rollup     prefix scans narrowing the group segment level by level
Graph walk       walk (one hop = one prefix scan)
```

## The GROUP BY recipe

### Index-sort grouping (read time)

Put the group field first in `fields(...)`; the sort IS the grouping:

```rust
#[kv_index(by_org { fields(org_id, created_at) })]
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
| `#[kv_reduce]` | compile time | reversible aggregate, maintained on the write path, resident | fixed groups, read-heavy |
| `group_by` (index sort) | read time | single-pass fold, varies per query | ad-hoc grouping, multi-dimension |
| reduce GROUP vs index-first-field | — | — | the same field encoders, different landing time |

Criterion: fixed group dimensions with hot reads → reduce (pay at
write, read free); varying dimensions → index sort + group_by (pay at
read, one layout covers many dimensions). Not exclusive: one index can
serve group_by and the timeline scan at once.

## The JOIN recipe: merge_join

```rust
// Both tables declare an index over the join key (key-ordered
// streams); merge_join merges in one pass.
let left = orders.scan::<Order_ByUser>(&[]);    // key-ordered
let right = users.scan::<User_ById>(&[]);       // key-ordered
let pairs = okm_query::merge_join(
    left.into_iter().map(|(k, _)| (user_bytes(&k), ())),
    right.into_iter().map(|(k, _)| (k, ())),
);
```

No hash join, deliberately: a join key that is not either side's sort
dimension is a modeling gap — declare `kv_index(fields(join_key))` and
the stream is ordered again. Sorting is a property of the store, not a
burden on the query operator (merge join stays streaming and
memory-bounded; hash join must materialize the build side).

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

## Boundary record: the two forms of WHERE

- **prefix = physical WHERE.** The byte range a prefix scan hits IS
  the storage-layer filter, free (non-hits are never read).
  Selectivity belongs in key layout — "how is this entity queried"
  decides what leads `fields`. Pushing a key-qualifying predicate into
  `.filter()` abandons the storage layer's selectivity: every query
  pays decode cost for rows it throws away.
- **`.filter()` = in-memory WHERE.** _stdlib suffices, no wrapper_:
  `.filter()` straight over the scan's `Vec<(PrefixKey, Option<R>)>`.
  Reserved for predicates that cannot enter a key (non-prefix
  dimensions, cross-field conditions, computed predicates).

One-line criterion: **first ask whether the predicate can become a
prefix of some access method; if yes → change the index declaration,
if no → filter**. A prefix is free; a filter pays per row.
