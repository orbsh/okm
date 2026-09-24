# ADR-0025: Two schema carriers, one keyspace — static codegen and dynamic schema-data modes

> **Languages:** [English](0025-runtime-ns-dynamic-collection.md) (primary) · [中文](0025-runtime-ns-dynamic-collection.zh-CN.md)

**Status:** Accepted (2026-09-24) — architecture record; the mechanism predates this ADR

> File-name note: this ADR was drafted as "runtime-ns dynamic collection"
> proposing a new okm-core collection type; the draft was WITHDRAWN the
> same day — okm-dynamic's `DynamicCollection` (runtime ns, schema-typed
> keys) already covered the need, and a second, order-preserving key wire
> in okm-core would have broken the byte-equality contract this ADR
> records. The ADR survives as the two-mode architecture record.

## Context

Aura's ADR-0026 (type-scoped actor storage) raised the question of which
okm surface a host uses when the namespace is a RUN-TIME value: every
registered actor type allocates one real ns from a registry, and the
application declares its collections in a schema carried at upload. Two
candidate answers appeared:

1. A new okm-core collection type whose keys are run-time field frames
   (an order-preserving wire, `KeyKind` lists carried to the decoder).
2. The EXISTING okm-dynamic `DynamicCollection`: run-time ns constructor,
   keys encoded from a `CollectionSchema` value — the same fixed-width BE
   discipline the derive produces.

Option 1 re-invented, and degraded: the dynamic mode already existed
(okm-dynamic, ADR-0022 lineage — schema-driven codec, bindings, shared
plan surface), and a second key wire would fork the byte-level contract.
The confusion it exposed is worth recording: WHERE the dynamic mode lives,
and how the two schema carriers relate.

## Decision

### 1. One engine contract, one key layout, TWO schema carriers

- **Static mode — code generation.** Rust types + okm-derive; the
  compiler is the schema validator (key widths, offsets, index
  declarations are compile-time facts); `Collection<S, K, R>` is the
  assembly point. For Rust hosts and wasm actors compiled from Rust
  source.
- **Dynamic mode — schema as data.** A `CollectionSchema` value (exported
  from the static side via `CollectionSchema::of`, or authored as data) +
  okm-dynamic. `DynamicCollection` takes a run-time ns and the schema
  value; encode/decode mirrors the derive byte-for-byte. For
  embedded-language actors (Python/Steel bindings) and hosts assembling
  collections at run time.

For the same declaration the two carriers produce IDENTICAL bytes: same
header discipline, same key encoding, same payload frames, same
dictionary behavior. Data written by one reads back through the other —
**mode is a property of the WRITER, not the data**. The external API is
aligned on purpose (put/get/scan/delete + document map), so application
code shapes do not fork.

### 2. The dynamic mode lives in okm-dynamic, not okm-core

okm-core hosts the SHARED runtime: the engine contract (`VirtualStorage`),
the compiled-mode assembly point (`Collection`), the document/dynamic-
segment machinery, reduce, subscribe. okm-dynamic hosts the schema-driven
codec and `DynamicCollection` — including its runtime-ns constructor
(present since the ADR-0022 lineage; no core change was ever needed).
okm-derive is the static codegen; okm-wire the remote wire format;
okm-query/stream/graph/vector/ngram are consumer-side operators above the
core. A keyspace primitive that only the dynamic mode uses does not enter
the core.

### 3. Aura consumption shape (ADR-0026)

- **python / steel actors**: `ctx.store.emit(op)` → the realm resolves
  the type's registry-allocated ns (`meta::ns_of`) and executes the op
  through okm-dynamic `DynamicCollection` built on the actor's declared
  schema.
- **wasm (Rust source) actors**: the static path compiled INTO the module
  — derive + `Collection` over a script-implemented `VirtualStorage`
  whose engine calls cross the host bridge. Full power: indexes, reduces,
  compile-time validation; no schema-data detour.
- Both write the same bytes at the same ns, so data is interoperable
  across carrier languages.

## Honest semantic cost

- **Two carriers, one drift surface.** Byte-equality is load-bearing and
  locked by cross-language tests; a derive change that does not update
  the dynamic codec (or vice versa) silently forks the layouts. The
  bindings' frame byte-equality acceptance tests are the guard.
- **The dynamic mode's schema is runtime data** — no compile-time
  validation of key widths or index declarations; malformed schemas
  surface as codec errors at execution, not at build.

## Consequences

- The withdrawn okm-core draft is removed; okm-dynamic remains the sole
  dynamic-mode home. README records the two-mode division at the root.
- Aura's executor work proceeds on okm-dynamic; no okm-core changes are
  required for ADR-0026.
