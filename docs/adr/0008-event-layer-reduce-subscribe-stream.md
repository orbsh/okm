# 0008 — Event layer: reduce, subscribe channels, okm-stream

> **Languages:** [English](0008-event-layer-reduce-subscribe-stream.md) (primary) · [中文](0008-event-layer-reduce-subscribe-stream.zh-CN.md)

**Status:** Implemented (2026-09-11: bare `#[kv_subscribe]` with build.rs-derived event enum, monotonic batch epoch, okm-stream combinators; per-row fallback channel rejected — see PLAN channel-payload decision)

## Context

Cross-row pre-aggregation (ADR-0005 slot space, shipped 2026-09-10) introduced the
`#[kv_reduce]` helper: a user-implemented `ReduceLogic` (Acc + fold/unfold) driven by
a read-modify-write hook on the write path. In reviewing what that hook actually is, a
generalization surfaced: fold/unfold is **inline consumption of a row-event stream**. The
same event source supports other consumers — triggers (stateless side effects), external
notification (observer), and FRP-style stream composition. This ADR records the decision to
generalize the mechanism and the boundaries that keep it honest.

## Decision

### 1. aggregate → reduce (rename, semantics unchanged)

`#[kv_reduce]` / `ReduceLogic` / `ReduceCodec` / `reduce_get` / `scan_reduces`
rename to `#[kv_reduce]` / `ReduceLogic` / `ReduceCodec` / `reduce_get` / `scan_reduces`.
The rename aligns the name with the role: a reduce is a stateful, reversible reduction over
the row-event stream (fold on put, unfold on delete). No behavior change; the two-layer
trait split (user implements Logic, derive implements the trait on it — coherence-driven)
is unchanged.

### 2. One event source, two consumer classes — the load-bearing boundary

The write path emits a **row event** (table identity, key, op: put/delete, row payload) at
the existing hook bottleneck (`Row::__okm_apply_*` call sites in `Table::put`/`delete`).
Two consumer classes attach to it, with deliberately different failure semantics:

- **Inline consumers** (`#[kv_reduce]`, triggers): synchronous calls in the write path,
  same ordering as the writes, exactly-once per event. Reduce's reversibility contract
  (`unfold(fold(a,x)) = a`) is only meaningful under exactly-once — a lost event is a silent
  acc/row mismatch (data corruption, not degradation). Inline is therefore not a channel
  consumer and never will be.
- **Channel consumers** (`#[kv_subscribe]`): each annotation point on a row type
  declares that the row's events enter the channel — the derive emits only a
  uniform-format send (row identity, key, op, payload) at the write path. No handler is
  attached at the annotation site: processing logic belongs entirely to the channel
  consumer; the stream crate's combinators are the adapter layer between raw events and
  that logic. Delivery is **best-effort with no guarantees** — bounded/unbounded and
  drop/block policy are the subscriber's declaration, not core's promise. This is the
  observer pattern's honest shape: notify tolerates loss.

Trigger asymmetry (documented, not unified): reduce is reversible (unfold); triggers are
at-most-once inline side effects with no transaction boundary — there is no undo for
"already notified" or "already wrote the other table". Two failure semantics on one event
source stay two things.

### 3. Channels live in core (derive-generated), not in an extension crate

- `#[kv_subscribe]` is a new attribute on `RowEncode` structs; it only declares that the
  row's events enter the channel — each annotated row type gets a uniform-format send
  (no handler at the annotation site; consumers decide the processing, combinators are
  the adapter).
- When ≥1 subscribe declaration exists on a row type, the derive emits a **global channel
  declaration plus a consumer-side accessor** (`okm_core::events::<R>()` shape). Proc-macros
  cannot own a true process global — the static is a per-row-type `OnceLock` in the
  generated module. Cross-table-type aggregation of streams is the stream crate's merge
  combinator, not core's job.
- Event format is uniform: (row-type identity, key bytes, op, payload bytes). Core defines
  the shape; handlers decode what they need.
- Async boundary: producer side is synchronous (`try_send`, µs-scale, no spawn in the write
  path); the consumer end is `tokio::sync::mpsc` (unbounded by default — loss tolerance is
  the declared semantics; bounded+policy is a later option). **Core stays synchronous.**
  Fjall's blocking hooks are the caller's embedding concern (`spawn_blocking` belongs to the
  embedding, not to okm-core); slatedb's async sits behind `KvEngine`; full-async infection of
  the core buys nothing and costs every API.

### 4. okm-stream: FRP combinators over the channel (new crate)

A new workspace crate `okm-stream` consumes the core-emitted receivers and provides the
Rx-style combinators — map/filter/merge/scan — plus push-mode multi-table fan-in (merge
several tables' streams into one reduce). It has **zero storage responsibility** and no
new primitives; pull-mode multi-table fan-in (scan + RMW, no event stream) remains in
`okm-query`. Naming: the crate is named for what it does (stream processing), not for where
the bytes come from; the bus lives in core.

### 5. okm → okm-core rename

The runtime crate renames to `okm-core` (workspace member, directory, crate name, all
`okm_core::` references). Derive crate stays `okm-derive`. **All references are updated,
including ADRs** (user decision 2026-09-10: 全部改) — ADRs remain decision archives, but
their code references are corrected so greps stay truthful. Ordering: the rename lands
first; every later phase builds on the new name.

## Alternatives considered

- **Unify trigger/notify/reduce as one channel-based mechanism.** Rejected: channel
  delivery cannot give reduce exactly-once, and faking it (bounded+retry) reintroduces
  distributed-protocol semantics the INTEGRATION boundary excludes. Inline vs channel is a
  structural split, not a preference.
- **Spacetimedb-style reducers (reducer = the write path, transactional stored
  procedures).** Same spirit (row events drive state), different structure: OKM reduce is
  declarative and auto-reversible (closer to materialized views); Spacetimedb reducers are
  hand-written write-path logic with client-side SQL subscriptions. Not adopted; do not
  conflate the two when comparing.
- **Event bus in okm-stream (extension crate owns the channel).** Rejected: then the write
  path would depend on an extension crate, inverting the layering. Core emits; stream
  composes.

## Boundaries

- Core stays synchronous end to end; async exists only at the channel's consumer end.
- Core promises mechanism only: channel exists, events are emitted in write order, shape is
  uniform. Delivery guarantees, filtering, transformation, fan-out across row types are
  downstream concerns (`okm-stream` or the application).
- Multi-writer/distributed event delivery remains out of scope (single-writer discipline,
  same boundary as reduce's RMW hook).
- No event replay/persistence: the channel is live-only; durable change data capture is an
  engine-layer capability (fjall watch) and stays out of the model layer.
