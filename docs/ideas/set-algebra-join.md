# Plan: the join is set algebra over ordered key streams, not an operator

**Revision 3**: the scan verbs collapsed to three (prefix / exact / range), all
returning lazy iterators — the eager forms were the same semantics collected, so
they are gone; and the operator family gained `merge`, the general alignment
(`(Some, None)` / `(Some, Some)` / `(None, Some)`), with `intersect` and
`difference` as its two named filters. Chaining is the composition: an arbitrary
number of sides, in the caller's order.

**Status**: **implemented 2026-11-11, revision 2 included** (§10: operators as methods, payload as a scan type parameter — `scan_entries`/`scan_covered` did not survive the sweep). What is left is §8's `mudra`/`krystallizer` sweep, which lives in those repos. It completes item 4 of [`simd-intersect-for-okm-query.md`](simd-intersect-for-okm-query.md) §6 ("no `intersect`/`union`; a named specialization would document intent, skip the sort for the already-ordered case") and takes items 1–2 with it on the join path. `group_by` is untouched there and still folds into a `Vec`.

## 1. Decision

Stop treating a join as one operator that produces rows. Split it in two, and put each half where it belongs:

- **Set algebra over keys** — `intersect` and `difference` as lazy iterator adapters over streams that are already ordered by the join key. Zero allocation, `take(n)` stops early, N sides by folding.
- **Ordinary code** — turning a shared key into rows: fetch by key, and the m×n cross product when a key repeats on both sides.

The second half is not an operator and should not become one. It is a `flat_map` over a `scan`/`get`, and the shape of it belongs to the caller who knows which tables are involved.

## 2. What is already true

**2.1 std has nothing to reuse.** There is no `join`, `intersect`, `difference` or `merge` on `Iterator`. What exists is `Peekable`, `zip`, `chain`, and `Iterator::is_sorted` (1.82, slice form earlier). `BTreeSet::intersection` is an ordered iterator but it *probes* the other set (`O(m log n)`), so it is not a merge and it requires building the sets first.

**2.2 itertools' `merge_join_by` is close but not the same operation.** It is lazy, it requires both sides sorted, and it yields `EitherOrBoth` — so intersect = keep `Both`, difference = keep `Left`/`Right`. What it does *not* do is the duplicate case: its `next()` pulls one element from each side per step, so equal keys are paired once and each element is yielded at most once. SQL semantics for a repeated key is the cross product of the two runs (m×n pairs), so adopting the dependency would not remove the interesting code — it would only remove the 40 lines around it. `kmerge_by` covers the N-way *ordered* merge but not the intersection.

**2.3 The order is already there.** An index scan's order is the entry tuple `(fields…, key)` in byte order, and any *prefix projection* of that tuple is still ordered. So join on the index's leading field ⇒ both streams arrive ordered by the join key ⇒ no sort, and the contract in §4 is free to satisfy. This is what the field segment (`I::Fields`, added in `16fb50c`) makes usable: the caller reads the join key off the entry instead of fetching the row to find it.

## 3. The three pieces

**3.1 `okm-core`: an entry-level scan (no fetch).**

```rust
pub type IndexHit<I> = (<I as KvIndex>::Fields, PrefixKey<<I as KvIndex>::Key>);

impl Collection {
    pub fn scan_entries<I>(&self, probe: impl ProbeBytes<I>) -> Result<Vec<IndexHit<I>>>;
    pub fn scan_entries_exact<I>(&self, probe: impl ProbeBytes<I>) -> Result<Vec<IndexHit<I>>>;
}
```

Both delegate to the free `scan_index` / `scan_index_exact` with `self.header()` (so a restored ns override keeps working) and return the same hits in the same order as `scan` / `scan_exact`, minus the payload read. Why this is the important half: intersecting needs `(fields, key)` only, while `scan` builds its answer out of documents — so today's join fetches every hit and throws most of them away. That is the SQL plan in reverse; this method is what fixes the order of operations.

**3.2 `okm-query`: the two set operations.**

```rust
pub fn intersect<L, R>(left: L, right: R) -> impl Iterator<Item = L::Item>
where L: IntoIterator, R: IntoIterator<Item = L::Item>, L::Item: Ord;

pub fn difference<L, R>(left: L, right: R) -> impl Iterator<Item = L::Item>
where L: IntoIterator, R: IntoIterator<Item = L::Item>, L::Item: Ord;
```

Both hold two `Peekable`s and advance the smaller `peek()`. On equality, take one copy out with `next()` (so the key needs no `Clone`), then drain the remaining copies of that key on both sides, so one key is answered once however many entries carried it. `difference` emits a left key only when the right side is exhausted or strictly greater, and drains both sides on equality.

N sides is a fold — `intersect(intersect(a, b), c)` — because the item type stays a key. That is the "only one join form" decision from the previous round, now with no builder to justify.

**3.3 `okm-query::merge_join` stays, demoted.** It keeps its `(K, V)` pairs, its own sort, and its run-expansion cross product — it is *the multiplicity step*, and the documented entry point for "give me both sides' rows for the keys that meet". Its doc comment is rewritten to say so and to point at `intersect` first. The existing pair-level `difference` is **removed** (§4: set semantics), not kept beside the new one.

## 4. Semantics

- **Set, not multiset.** A key carried by several entries on either side is answered once. That is `intersect` and `difference` alike, and it is the reason the pair-level `difference` goes away.
- **m×n is downstream, and only exists there.** At the key level there is no multiplicity at all. It appears when a key becomes rows: m rows on the left, n on the right, m×n pairs. The implementation touches multiplicity only to *drain* runs — consumption, not output.
- **Outer joins are two differences.** `difference(left, right)` is left-only, `difference(right, left)` is right-only, `intersect` is the meeting. "List the rows that exist on one side only" needs no third operator, and the three answers partition the two input streams (§6 test).
- **The ordered contract is a contract, not a check.** Verifying it would mean reading a stream to its end before answering — the materialization the adapter exists to avoid. Unordered input therefore does not error: it silently misses keys. The doc comment says this in its first paragraph, and a test pins the failure mode so nobody later mistakes it for a bug.

## 5. From keys to rows

Two shapes, both documented in `query-recipes` with their costs:

- **Re-probe per key** — `common.flat_map(|k| table.scan::<ByK>(k)).map(...)`. One index probe per shared key; the key set can be filtered or truncated first, so selectivity applies; everything stays streaming. The right default.
- **Lockstep over pairs** — feed `(key, pk)` from both sides to `merge_join`. One pass, no probes, but it materializes and reads both sides' payloads. Cheaper only when nearly every key is shared.

m×n appears in both, in the caller's nested loop, which is where the multiplicity is visible instead of buried.

## 6. Tests

`okm-query`:

1. `intersect` yields each shared key once, with duplicates on both sides;
2. three-side fold equals pairwise intersection;
3. laziness: a counting iterator shows `take(1)` pulls fewer than 20 items out of 1000 — the only way to see laziness from outside;
4. `difference` both directions, duplicates, and the exhausted-right side;
5. `intersect` + both differences partition the two sides (`left = left_only ∪ common`).

`okm-core`:

6. `scan_entries` equals `scan`'s `(fields, key)` element for element (same hits, same order, one read less), a group probe still decodes the whole segment, and `scan_entries_exact` rejects a value that merely extends the probe.

## 7. Documents

- `docs/query-recipes.{md,zh-CN}`: the JOIN section becomes the three-step pipeline (entries → intersect keys → fetch survivors), keeps the A/B cost table of §5, and states that the cross product lives in the fetch step. The claim that `merge_join`'s sort is the only order story goes away with the sort.
- `docs/internals/index-mechanism.zh-CN.md`: the scan walk gains the entry-level variant (same hits, no `Option<R>`), and its "what a scan hands back" line gains `scan_entries`.
- `docs/MODELING.{md,zh-CN}`: one sentence in the index section — the entry-level scan is a join's first step, since a join key is a column the entry already carries.
- `docs/PLAN.md`: one ledger entry: the operator stops at the key set, multiplicity belongs to the caller.
- `docs/ideas/simd-intersect-for-okm-query.md` §6: items 1, 2 and 4 marked done, with what is still open there (`group_by` allocations).

## 8. Impact

`okm-query` has no downstream consumers: `krystallizer` depends on `okm-core`/`okm-lake`/`okm-value`/`okm-derive`/`okm-vector`, `mudra` on `okm-core`/`okm-wire`/`okm-derive`/`okm-ngram`. So the signature change (`difference` losing its values, `intersect` appearing) needs no compatibility layer.

Follow-up in `mudra`, not in this change: `pages_of_tag` currently calls the free `scan_index_exact` with a hand-passed store and ns prefix; with `Collection::scan_entries_exact` it can go through the handle.

## 9. One gap found while planning

`PrefixKey<A>` derives nothing (`okm-core/src/model/key.rs:240`), so `IndexHit<I>` cannot be compared or printed: an equality assertion on two `Vec<IndexHit>` does not compile, and a failing assertion cannot show the value. `#[derive(Clone, Debug, PartialEq, Eq)]` is wanted before the tests in §6.6 are writable — a small, separate change.

## 10. Revision 2: methods, and one scan with the payload as a type parameter

Two things the first draft got wrong, both about shape rather than semantics.

**10.1 The operators are methods.** `a.intersect(b)`, `a.difference(b)` — a trait
implemented for every `Iterator` (legal: the trait is ours, the type is not ours to
change). The receiver carries the direction, so `difference` needs no rename: it is
`a \ b`, read as "everything in `a` that `b` does not have", and that is exactly
`a − (a ∩ b)` without computing the intersection first. The chain in the first draft
(`a.intersect(b).difference(a)`) was wrong twice over — `a` is moved, and the order
is inverted. Left-only is `a.difference(b)`; right-only is `b.difference(a)`;
"both halves plus the meeting" is three calls on the same two sources.

Renaming to `subtract`/`minus` would read better in English prose and worse in a
document that already says "set difference"; `intersect` and `difference` stay.

**10.2 One scan, with what to read as a type parameter.** `scan_entries` was a second
payload semantics beside `scan`, which is precisely the duplication to avoid. The
payload is not a property of the *scan* — it is a property of the *question*, so it
becomes a type parameter, and the verbs keep only the axes that really are theirs:
probe shape (prefix, exact, range) and laziness (eager, iterator).

```rust
/// One part of a hit. Order and count belong to the caller: fewer parts is less
/// work, and there is exactly one scan method, not one per arity.
pub struct Fields;  // I::Fields, decoded from the entry's sort segment
pub struct Key;     // PrefixKey<I::Key>, the tail
pub struct Row;     // Option<I::Document> — the ONLY part that reads the store
pub struct Include; // I::Covered, decoded from the entry value (() when none)

pub trait Part<I: KvIndex> { type Output; fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output>; }
pub trait Parts<I: KvIndex> { type Output; fn take(hit: &mut Hit<'_, I>) -> Result<Self::Output>; }   // one impl per arity, ceiling = the number of parts

t.scan::<ByOrg, (parts::Key, parts::Fields)>(&probe)?;  // no fetch, caller's order
t.scan::<ByOrg, (parts::Fields, parts::Key, parts::Row)>(&probe)?;   // the plain answer
t.scan::<ByOrg, (parts::Key, parts::Include)>(&probe)?;             // the covered answer
```

The parts are named through the `parts` module rather than imported bare: `Key`,
`Fields` and `Row` are ordinary words, and a document that already has a struct
called `Row` should not have to rename it to use a scan.

The six payload-encoding verbs go: `scan` stays `(Field, Key, Row)`, and
`scan_entries`, `scan_entries_exact` and `scan_covered` become part tuples. What
remains is one method per probe/laziness axis: `scan`, `scan_exact`, `scan_range`,
`scan_range_iter`, `scan_prefix_iter`.

**Cost — runtime.** Nothing. An unrequested part is a type that is not in the tuple,
so it is not in the monomorphized body: no branch, no `Option`, dead code
elimination rather than an `if`. A requested part costs what it costs today, and
`Row` is the only one that reaches the store. The saving is real in the other
direction: `(Key, Field)` skips the field decode that `scan` does today even for a
caller that only wanted keys, and `(Key)` alone skips both decode and fetch.

**Cost — everything else.** Two, and both are honest. (1) A function has no default
type parameter, so every call site spells its parts: ~120 in this repo, 17 in
`mudra`/`krystallizer`, one sweep each. (2) Each verb × each part-tuple used
monomorphizes separately, so code size grows with the number of distinct questions
asked — bounded in practice by the parts menu (four real parts), unlike a generic
over the payload type.

If that churn is not wanted, the alternative is `scan` keeping its fixed triple plus
a `scan_parts` beside it — which is the two-semantics shape this revision exists to
remove. Recommendation: take the churn once.

## 12. Non-goals

- No chained builder (`Ns1.join(Ns2).on(..)`) — it would cover two sides and hide the key, which is the one thing the adapter needs explicit.
- No hash join: no hashing of the key type, `O(n)` memory instead of a build side.
- No runtime sortedness check (§4).
- No lazy *row* join: a lazy cross product would have to buffer a run anyway, and it would put multiplicity back inside the operator.
- No `union` yet: `chain` plus a dedup pass is the shape, and nothing asks for it.

## 13. Order of work (revision 2)

1. `PrefixKey` derives (§9) — done.
2. The parts menu: `Part`/`Parts`, `Field`/`Key`/`Row`/`Include`/`__`, the five verbs
   taking `<I, P>`; `scan_entries`, `scan_entries_exact`, `scan_covered` removed;
   every call site in this repo rewritten. The existing `index_test` cases become the
   proof that the parts answer what the removed verbs answered.
3. `intersect`/`difference` as methods (§10.1) + tests 1–5; the pair-level `difference`
   is gone; `merge_join`'s doc points at `intersect`.
4. Documents (§7) — the payload menu, not `scan_entries`.
5. `mudra`'s `pages_of_tag` becomes `scan_exact::<(Key)>`; `mudra` + `krystallizer`
   take the `<I, P>` sweep (other repos).