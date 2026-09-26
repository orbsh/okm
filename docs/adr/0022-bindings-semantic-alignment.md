# ADR-0022: Bindings semantic alignment — the dynamic-mode capability ceiling is scoped, not permanent

Date: 2026-09-22
Status: Accepted (supersedes the "permanent capability ceiling" ruling of the dynamic codec, PLAN Phase 8 `[~] Dynamic codec` item and the doc-comment ceilings in okm-dynamic; ADR-0008's event-layer design itself is untouched).

## Context

The dynamic codec ruling (PLAN Phase 8, 2026-09-12) established okm-dynamic and the Python/Steel bindings as schema-driven encode/decode surfaces, and drew a capability ceiling: **no reduce, no subscribe, no function indexes** — semantics stay Rust-side compile-time, because "a dynamic rebuild would break exactly-once". The ceiling was recorded as permanent in the PLAN item, the okm-dynamic doc comments, and was attributed to ADR-0008's event-layer framing.

Two things were conflated in that ruling. Separating them is the reason it changes.

### What ADR-0008 actually rules

ADR-0008 rules on the **event layer**: reduce/subscribe are inline/channel consumers of a row-event stream, and the fold hook lives inside the write path's engine-locked read-modify-write. Its real invariant is: *fold and the document write must see the same engine state — the exactly-once property of the accumulator update*. Nothing in ADR-0008 says the fold function must be Rust, or that it must be executed inside okm-core. ADR-0008 never mentions bindings or the dynamic mode; the "per ADR-0008" citation in okm-dynamic's ceiling note was an over-extension of the exactly-once argument to a claim about implementation language.

### What actually changed since the ruling

The ceiling was written when the bindings' only user was hypothetical. Two concrete deployment shapes have since emerged, and neither is served by an encode/decode-only binding:

1. **Standalone Python applications.** fjall has no Python binding and raw KV is unwieldy; okm-dynamic is the answer — a semantic KV facade over a Python-held engine. In this shape the Python side is the ONLY writer; there is no "other side" to interoperate with. Telling this user "your writes cannot maintain index entries or reduce groups" excludes the primary consumer of the crate.
2. **Aura booths in Python.** An booth produces okm operations and sends them to Aura for execution against nested storage. The booth's language is a deployment fact, not an interop requirement.

The old reasoning assumed semantics require Rust compile-time code and therefore cannot exist on the binding side at all. That premise is wrong for function-shaped semantics: a function pointer is language-agnostic — what matters is that the same semantic contract is upheld, not which runtime executes it.

## Decision

**The bindings implement semantics. Each semantic surface is realized as a host-language callable held at binding registration time; callables execute locally, and the remote path carries semantic RESULTS as operation payloads, never callables.**

### Route: host-language callables, binding-time registration

The declaration-side information of every semantic surface is already data (associated consts): `KvIndex`'s `SLOT`/`FIELDS`/`INCLUDES`/`KEY_PREFIX`, `Reduce`'s `SLOT`/`GROUP`, and `CollectionSchema` + `SlotMap` export all of it. The only thing that was ever Rust code is the semantic function itself — and that is exactly the part a binding can supply in its own language:

- **Function indexes**: `Schema.add_func_index(name, fn, includes=[])` — the callable maps a decoded document to an encoded value or an iterator of values (multi-entry fan-out, the inverted-index regime). Partial-index `admits` is the same shape: a predicate callable.
- **Reduce**: `Schema.add_reduce(name, group_fields, fold, unfold, acc_codec)` — the callables implement `ReduceLogic`'s fold/unfold in the host language; the accumulator codec is a declared byte-layout rule (u64 = 8B BE is the seed; compound accumulators ride the byte-transparent form, same as Rust's `Vec<u8>` escape hatch).
- **Subscribe: stays excluded.** Event emission is a write-path broadcast with consumer-side protocol (epoch folding, channel registration, okm-stream composition). It is not a per-document derivation and gains nothing from a callable; the ceiling remains for this one surface.

### The invariant that replaces the ceiling

The exactly-once concern of ADR-0008 is preserved by a deployment-shape contract, not by an implementation-language ban:

- **Embedded mode (Python owns the engine)**: single writer by construction. Fold/unfold run in-process inside the binding's put/delete, which must replicate the Rust write path's calling discipline exactly — put folds the new document, delete unfolds the stored one, overwrite unfolds the old document then folds the new one, all sharing the put path's engine batch. The calling discipline, not the language, is what the correctness rests on; a wrong call site is a silent accumulator drift, so it is the acceptance test's target.
- **Remote mode (Aura: booth executes locally, remote executes mechanically)**: callables run in the booth's own runtime; operation payloads carry semantic RESULTS — derived entry bytes, absolute accumulator values ("acc becomes 42"), and the old document (or its version) for overwrite unfold. The remote executes them as one framed batch (ADR-0010 §2's single `commit_batch`), so document write + accumulator update stay atomic without the remote understanding either. The remote never hosts callables, never parses semantics — ADR-0010's receiver contract is untouched.

The ruled-out alternative — delegating function execution to the remote environment or embedding callables in operation payloads — is rejected on both complexity and principle: it makes the remote a runtime for foreign code, inverting ADR-0010 §7's "receiver hosts an engine, not execution" and re-coupling the wire to host runtimes.

### The cost that IS accepted, stated plainly

Local execution of reduce across a remote link changes the consistency envelope in one case: **multiple writers to the same reduce group**. The Rust-native fold is engine-lock-atomic per writer; a locally-executed fold computes from a local view of the accumulator, so two writers folding the same group concurrently lose updates. This is resolved by the ownership rule:

- **Single-writer-per-group is a precondition of the remote reduce mode.** Aura's partitioned booth model satisfies it by construction (an booth owns the data it writes). Under the same rule the booth may cache the accumulator locally — it IS the authoritative value — so steady-state puts carry an absolute acc and cost zero extra round trips; only restart recovery reads the acc once (an explicit contrast with the Redis-style shared-RMW pattern, which needs CAS/transactions precisely because it has multiple writers).
- Multi-writer groups do not fit this mode. Their semantic execution belongs in the owning process (Rust-side derive, or a dedicated owner booth). This is a scope boundary, not a limitation to be engineered away.

### Semantic alignment, not interop

Rust and Python functions implementing the same declared surface never cross a runtime boundary: no callable is serialized, shipped, or executed on the other side. "Alignment" therefore means **semantic equivalence against the contract**: the same declared schema, driven from either side, produces the same entries, the same accumulator evolution, the same unfold compensation. Acceptance:

1. Embedded mode: a schema declared on the binding side, driven purely from Python, exhibits contract-conformant semantics — the calling-discipline test (put/delete/overwrite against the accumulator) is the core case.
2. Remote mode: operations produced by a Python booth, executed on a remote engine, land byte-identically to the same operations produced Rust-side (the existing cross-language byte-equality tests, extended from codec bytes to semantic entries).

## Consequences

- okm-dynamic gains callable-carrying surfaces: `AccessMethod` grows func/admits variants; `DynamicCollection::put`/`delete` grow the reduce calling discipline. The byte-layout work is small (the encoders exist); the calling-discipline work is where correctness lives.
- The "permanent ceiling" wording in okm-dynamic's doc comments and the PLAN item is corrected to this ADR's scoped ruling: **subscribe excluded; everything else callable-implementable under the deployment-shape contract**.
- ADR-0008 is not modified: its event-layer design holds for the Rust-native path, and its exactly-once invariant is what this ADR re-derives as a deployment-shape contract instead of an implementation-language ban.
- Rejected-for-now, not rejected-ever: subscribe alignment. If a host-language consumer story emerges (an booth wanting reduce-group events in-process), it reopens with the channel protocol as the design surface.
