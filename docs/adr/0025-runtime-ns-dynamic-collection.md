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

Aura's ADR-0026 (type-scoped booth storage) raised the question of which
okm surface a host uses when the namespace is a RUN-TIME value: every
registered booth type allocates one real ns from a registry, and the
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
  assembly point. For Rust hosts and wasm booths compiled from Rust
  source.
- **Dynamic mode — schema as data.** A `CollectionSchema` value (exported
  from the static side via `CollectionSchema::of`, or authored as data) +
  okm-dynamic. `DynamicCollection` takes a run-time ns and the schema
  value; encode/decode mirrors the derive byte-for-byte. For
  embedded-language booths (Python/Steel bindings) and hosts assembling
  collections at run time.

For the same declaration the two carriers produce IDENTICAL bytes: same
header discipline, same key encoding, same payload frames, same
vocabulary behavior. Data written by one reads back through the other —
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

- **python / steel booths**: `ctx.store.emit(op)` → the realm resolves
  the type's registry-allocated ns (`meta::ns_of`) and executes the op
  through okm-dynamic `DynamicCollection` built on the booth's declared
  schema.
- **wasm (Rust source) booths**: the static path compiled INTO the module
  — derive + `Collection` over a script-implemented `VirtualStorage`
  whose engine calls cross the host bridge. Full power: indexes, reduces,
  compile-time validation; no schema-data detour.
- Both write the same bytes at the same ns, so data is interoperable
  across carrier languages.

### 4. The schema is not stored in the keyspace (2026-10-17)

Neither carrier persists its schema. `DynamicCollection` holds the
`CollectionSchema`, access methods and reduce specs it was constructed
with; the static `Collection` holds the derive's compile-time tables. The
only declaration-flavoured bytes OKM ever puts in the keyspace are name
vocabularies — the field-name vocabulary (fixed faces 0x02/0x03, ADR-0012)
and a graph's PREDICATE vocabulary (its own 0x04/0x05, ADR-0017 §3) — and
those record FACTS (names that were actually written, first-seen
allocation), not declarations. Nothing recovers key widths,
field offsets, cold tags, hot/cold placement, defaults or the `SlotMap`
from the keyspace.

The schema belongs to the writer, and a persisted COPY belongs to whoever
allocates the ns — not to the data keyspace:

- **The data plane is semantics-free.** A receiver prepends its declared
  prefix and executes bytes (ADR-0010); a schema record would make the
  storage layer a participant in OKM semantics again.
- **Byte equality.** The same declaration produces the same bytes from
  either carrier, and mode is a property of the writer (§1). A schema
  record is a write one carrier would not produce — or one both must, for
  no data reason. Either way it is a new divergence surface.
- **Precedent.** A persisted slot map was rejected as violating the
  code-only ns/slot vocabulary. Slot allocation is declared; a schema is
  more declaration than a slot.
- **Version evolution.** The data plane's version problem is already
  carried by the payload layout version (the writer's concern);
  declaration evolution belongs to the ns allocation. A schema record in
  the keyspace would put declaration migration on the keyspace too.

The reference landing is Aura's: the introspected schema rides the
booth-definition row's dynamic segment — one entry, native frames, never
JSON text — beside the rows it describes, with the type's ns and schema
resolving in one read (aura ADR-0025 meta-plane-fate + ADR-0040 bands).

## Honest semantic cost

- **Two carriers, one drift surface.** Byte-equality is load-bearing and
  locked by cross-language tests; a derive change that does not update
  the dynamic codec (or vice versa) silently forks the layouts. The
  bindings' frame byte-equality acceptance tests are the guard.
- **The dynamic mode's schema is runtime data** — no compile-time
  validation of key widths or index declarations; malformed schemas
  surface as codec errors at execution, not at build.
- **A keyspace alone is not self-describing.** The two dictionaries
  recover the names that were written, never the layout; a reader without
  the declaration record holds bytes it cannot decode. Export and backup
  carry the schema alongside the data (§4) — and a keyspace that must
  explain itself is an archive question (who owns its meaning), not a
  missing feature.

## Consequences

- The withdrawn okm-core draft is removed; okm-dynamic remains the sole
  dynamic-mode home. README records the two-mode division at the root.
- Aura's executor work proceeds on okm-dynamic; no okm-core changes are
  required for ADR-0026.
