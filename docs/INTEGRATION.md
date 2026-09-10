# Integration Boundaries: Extension Types and Primitives

okm has exactly three primitives: **ordered prefix scan** (the data
segment is the sort dimension), **function index** (the data segment is
a per-row derived value, precomputed at write time), and **double-written
edges** (first-class relations). This doc answers how the "advanced"
capabilities — FTS, vectors, graph algorithms — land on those primitives
(the `okm-ngram` / `okm-vector` / `okm-graph` crates), and — equally
important — which extension types do not land, and why.

## Core thesis

None of the three capabilities needs a new storage primitive:

```text
FTS     = multi-value function index (n-gram/token → N entries)
          + caller-side BM25 rerank
Vector  = single-value function index (fixed-width embedding bytes)
          + quantized-bucket prefix scan + edge entries as the ANN graph
Graph   = edge entries (neighbors = one prefix scan)
          + ordered-stream folds (label propagation / PageRank)
```

Algorithm work (scoring, iteration, convergence) lives entirely in the
integration crates or the caller; okm only guarantees the ordered
stream. The test for any new extension type: is it a **recipe over the
primitives** (→ integration crate) or a **new primitive** (→ core)?

## The two layers of precomputation

"Precompute" is used loosely; it is actually two different things:

**Per-row derivation — that IS the function index.** `lower(name)`,
`hour(ts)`, embeddings, tokens: computed once at write, zero cost at
read. The essence of `func` is moving read-side cost to the write side.
The exchange is symmetric: every put/delete pays one function call (N
for multi-value), and the read path stays a pure scan.

**Cross-row aggregation — not func; it is the mutable-counter path on
the value side.** func's signature is `fn(&Row)` — it sees one row.
Rollups (hourly counts), exact counters, moving averages span rows.
Their landing point is **read-modify-write on the entry value** — and
that is the opposite of the index-entry discipline:

- Index entries: append-only, born and die with the row (delete
  regenerates the exact set via the same function — nothing dangles);
- Counter entries: mutable values with an independent lifetime (the row
  is gone, the count remains — or tombstone logic is needed).

That makes cross-row precomputation a **fourth primitive** (mutable
aggregation entry). okm's stance splits in two layers: **core still
ships no aggregation semantics** — no built-in counter/sum types, no
distributed add protocol; but the mechanical half is provided as a
helper facility — the `#[kv_aggregate(Logic { group(a,b) })]`
declaration, a user-implemented `AggregateLogic` (fold/unfold plus Acc
encoding), and a read-modify-write hook on the write path (fold on put,
unfold on delete). Reversibility (`unfold(fold(a,x)) = a`) is the
implementor's contract; non-invertible aggregates (median, distinct)
do not qualify. okm does no zero-value GC — an emptied group keeps its
entry. Usage is documented in the modeling guide's cross-row
pre-aggregation section.

The concurrency boundary is unchanged: under single-writer engines the
hook is a safe read-modify-write; multi-writer races and distributed
add protocols remain outside the "ordered byte stream" model — a real
OLAP/stream system next to the KV is still the answer there.
Approximate counting (UV, hot terms) should prefer the degradation to
"entry exists = count 1" plus counting entries in a scan range.


## Extension types that land (present or crate-worthy)

- **Time series / windowing** — timestamp BE fixed-width in the data
  segment (byte order = time order); prefix scan = time window;
  `group_by` folds rollups. Zero new code — a modeling technique, not a
  crate.
- **Leaderboard** — score BE fixed-width + `Reverse<T>` for descending
  order, primary key tail for tie-breaking; `scan` = range by score.
  Also zero new code.
- **Autocomplete** — the field itself as the data segment; prefix scan
  is completion. One `fields(...)` declaration; no code warranted.
- **Geo** — Geohash string as the data segment; prefix scan = spatial
  range (a Geohash prefix is a rectangle). Structurally identical to
  n-grams (string prefixes); a crate only if demand materializes.

## What does not land (explicitly out of scope)

- **Hash join / unordered join acceleration** — a join key that is not a
  sort dimension is a modeling gap; declare an index and the stream is
  ordered again. `okm-query::merge_join` is the only answer.
- **Streaming subscriptions / CDC** — no tail-scan primitive exists;
  this belongs to the engine layer (fjall watch / slatedb invalidate),
  not a model-layer imitation.
- **Distributed add protocols** — under a single writer the
  `#[kv_aggregate]` read-modify-write hook is safe; multi-writer races
  and distributed counter protocols remain outside the "ordered byte
  stream" model — a real OLAP/stream system beside the KV.
- **General second-level cache** — invalidation policy is application
  logic; okm entries live and die with declarations, and there is no
  place to hang invalidation hooks.

## Discipline for the integration crates

`okm-ngram` / `okm-vector` / `okm-graph` share three constraints:

1. **Zero storage responsibility** — no private table structures, no
   bypassing `KvIndex`/`EdgeEncode`; all data flows through declared
   entries.
2. **Replaceable algorithms** — the crates ship minimal working recipes
   (n-gram + BM25, bucket scan + rerank, LP + PageRank); heavier
   solutions (Tantivy, arroy/HNSW, Louvain) are the caller's to attach
   against the same entry layout.
3. **Minimal dependencies** — no external dependencies unless truly
   needed (e.g. an ANN library); when needed, they live in that crate,
   never in core.
