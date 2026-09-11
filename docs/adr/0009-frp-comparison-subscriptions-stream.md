# 0009 — FRP comparison: Spacetimedb subscriptions vs okm-stream

> **Languages:** [English](0009-frp-comparison-subscriptions-stream.md) (primary) · [中文](0009-frp-comparison-subscriptions-stream.zh-CN.md)

**Status:** Accepted (analysis record — no code change implied; complements ADR-0008's
boundary clauses with the Spacetimedb reference point)

## Context

ADR-0008 fixed the event layer's shape: inline consumers (reduce, exactly-once, write-path
synchronous) and channel consumers (`#[kv_subscribe]`, best-effort, combinators in
`okm-stream`). Spacetimedb's client subscriptions are the closest widely-known system to
compare against — they are, structurally, an FRP graph that grew inside a database. This
ADR records the comparison so later design discussions don't silently import Spacetimedb's
reducer/subscription semantics into OKM's different shape.

## Decision

### 1. Same skeleton: a push-mode dataflow graph

Both systems are the same species — source → dependency → propagation → observer:

```
FRP:        source signal ──map/filter──▶ derived signal ──▶ observer
Spacetime:  table delta ──query-plan eval (IVM)──▶ subscribed row set ──▶ on_insert/on_delete
okm:        row event ──combinators (okm-stream)──▶ folded state ──▶ subscriber logic
```

The client cache is a signal (an observable cell holding the latest value); the callbacks
are observers; the SQL query is the declarative spelling of combinators (`where` ≈
`filter`, `join` ≈ `combineLatest`) — except the evaluation engine lives on the server and
the composition is expressed as a query plan instead of a code chain.

### 2. Glitch handling: transaction boundaries ARE the FRP clock

A glitch is a downstream consumer observing an inconsistent intermediate state — two
upstreams changed, a merge node fires on the first input alone, computes a dirty value
from half the data, then recomputes. Classic FRP kills glitches with topological ordering
plus atomic (batched) propagation.

Spacetimedb's documented mechanisms are exactly that, under database names:

- "Each database transaction generates exactly zero or one update message … atomic" —
  transaction-level batching = per-tick batched propagation; no subscriber ever sees half
  a transaction.
- "Callbacks … are deferred until the cache updates from a transaction are fully applied" —
  observers read post-propagation consistent state only, the glitch-free guarantee.
- Initial subscription snapshots are taken "between two transactions" — consistent read.

The structural advantage: a serial transaction log is a ready-made logical clock — commit
is the tick. RxJS lacks this and leaves glitch avoidance partly to the developer.

### 3. Combinator placement: the load-bearing difference

Where composition is evaluated caps what composition can express:

- **Spacetimedb** — server-side query engine, incrementally evaluated (IVM: "taking the
  derivative of the query"). Best experience: the network carries deltas, the client cache
  needs zero computation. Hard cap: the query must be analyzable — join subscriptions
  require indexes on both join columns (drop one and the subscription fails at client
  runtime); aggregation is not incrementally maintained (aggregations belong to views,
  and views are black-box code re-evaluated via read-set invalidation, not IVM).
- **okm-stream** — consumer-side, arbitrary Rust combinators over raw row events.
  No analysis needed, no expressiveness cap; the price is raw events on the wire and
  subscriber-side compute.

The trade is zero-sum: **wherever evaluation authority sits, that side's capability caps
the compositional language.** Spacetimedb can afford server-side placement because a
query engine is its native organ; okm-stream places composition at the consumer because
OKM is a library with no query engine to lean on.

### 4. Time semantics: discrete only, and signal vs stream

Spacetimedb subscriptions are discrete-event FRP: the world updates in transaction-sized
ticks; there is no value "between transactions". Continuous Behavior (classic FRP's other
half) cannot exist in a distributed setting — no global "now" to read.

One more axis: the client cache has **signal semantics** (readable current value); OKM's
mpsc channel is a **stream** (readable next event, no current value). Lifting a stream
back into a signal requires folding — which is precisely what inline reduce is: the
`scan()` combinator of Rx, placed in core because exactly-once and reversibility must be
enforced at the write path. Reduce is "turn the event stream back into a signal,
materialized in KV".

## Boundaries

- OKM does not subscribe to queries. "Compose at the write side, ship results" would mean
  growing a query engine + lineage analysis + IVM inside OKM — a different system layer,
  the same boundary that keeps stream systems out (now with its FRP proof: the
  expressiveness/analysis cap is structural, not a missing feature).
- Testable corollary: demand for server-side composition among okm-stream users is the
  early-warning signal that the library boundary is being violated.
