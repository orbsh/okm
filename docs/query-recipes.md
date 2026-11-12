# Query Recipes: okm-query Combinators and Their SQL Shapes

A recipe collection for okm-query: how the three combinators
(intersect / difference / merge_join / group_by / walk) compose into shapes familiar from SQL,
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
JOIN             intersect (ordered key streams) + merge_join for m×n
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

## The JOIN recipe: intersect keys, then fetch

A join is two steps, and only the first is a query operator.

**Step 1 — intersect the entries.** Both sides declare an index over the join
column, so both streams arrive ordered by it (an index scan's order is the entry
tuple `(fields…, key)`, and any prefix projection of that tuple is still ordered).
Ask the scan for the parts the intersection needs and nothing more — the type
parameter is the payload menu, so leaving `Row` out means no payload is read:

```rust
// ordered by user, then by key; (fields, key) — no fetch
let left = orders
    .scan::<Order_ByUser, (parts::Fields, parts::Key)>(&[])
    .map(|hit| hit.map(|(f, _)| f.user_id));
let right = users
    .scan::<User_ById, (parts::Fields, parts::Key)>(&[])
    .map(|hit| hit.map(|(f, _)| f.id));

let common: Vec<u32> = left.intersect(right).collect::<Result<Vec<_>>>()?;
```

A scan yields entries one at a time (`Result` per entry), so nothing is materialized
until you ask: `collect::<Result<Vec<_>>>()` is the eager mirror, `.take(n)` is a
LIMIT, `.rev()` walks backwards. There are three verbs, not eight — prefix, exact,
range — and the payload is the type parameter, so no verb exists per payload.

`intersect` is a method on anything that yields keys (`Vec`, array, scan iterator)
and a lazy merge over the two ordered streams: set semantics (a key that both sides
repeat is answered once), zero allocation, `take(n)` stops early. More than two
sides chains — `a.intersect(b).intersect(c)` — and so does the mixed question
"keys a and b share that c lacks": `a.intersect(b).difference(c)`.

The general form is `merge`, which answers one tuple per distinct key —
`(Some, None)` left only, `(Some, Some)` both, `(None, Some)` right only:

```rust
for (left, right) in left_keys.merge(right_keys) {
    match (left, right) {
        (Some(a), Some(b)) => both.push((a, b)),
        (Some(a), None) => left_only.push(a),
        (None, Some(b)) => right_only.push(b),
        (None, None) => unreachable!("merge never answers two absences"),
    }
}
```

`intersect` and `difference` are exactly the two filters over that stream, named
because they are asked far more often than "both sides, all three cases".

**Step 2 — fetch the survivors.** For each shared key, read the rows that carry it;
the cross product of a repeated key happens here, in this loop, and nowhere else:

```rust
for key in common {
    for order in orders.scan::<Order_ByUser, (parts::Fields, parts::Key, parts::Row)>(&key.to_be_bytes()) {
        for user in users.scan::<User_ById, (parts::Fields, parts::Key, parts::Row)>(&key.to_be_bytes()) {
            emit(order?, user?);   // m rows × n rows for this key
        }
    }
}
```

One index probe per shared key. Because the key set is complete before the first
row is read, it can be filtered, truncated, or counted first — selectivity applies
before any payload is touched.

**The other shape: lockstep, when nearly every key is shared.** If both sides
intersect almost fully, the per-key probes are the cost, and pairing the two
streams in one pass wins even though it materializes and reads both sides' payloads:

```rust
let pairs = okm_query::merge_join(left_pairs, right_pairs);   // Vec<(OrderKey, UserKey)>
```

That is the multiplicity step by definition (m×n pairs is its answer), so it returns
a `Vec` and sorts its own inputs: an already-ordered side costs one linear pass, and
an unordered one costs `O(n log n)` rather than a wrong answer. Choose it when the
probe count would exceed the row count. No hash join, deliberately: a sort-merge
needs no hashing of the key type and holds `O(n)` instead of a build side.

**Outer joins are differences.** "Left has it, right does not" is `a.difference(b)` —
the receiver is the left side, so `a − (a ∩ b)` needs no intersection computed
first — and the three answers partition the two sides:

```rust
let left_only:  Vec<_> = left.clone().difference(right.clone()).collect();
let right_only: Vec<_> = right.difference(left).collect();
```

For N sides, intersect once and subtract per side: `a.difference(common)`,
`b.difference(common)`, `c.difference(common)` — each answer is that side's "more
than the intersection".

**What the ordering contract costs.** `intersect` and `difference` require their
inputs to be ordered by key and do not check it: verifying would mean reading a
stream to its end before answering, which is the materialization they exist to
avoid. Unordered input does not misbehave loudly — it silently misses keys. The
contract is free when the join column is the leading field of the index (any prefix
projection of the scan tuple is ordered); when it is not, sort explicitly first and
the sort is visible in the code instead of hidden in the operator.


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
