# ADR-0023: Preset reduce combinators — `Count`, `Max`, `Min`, `Sum` as library declarations

> **Languages:** [English](0023-preset-reduce-combinators.md) (primary) · [中文](0023-preset-reduce-combinators.zh-CN.md)

**Status:** Accepted (2026-09-22); shipped 2026-09-23, with amendments below

> **Amendments (2026-09-23, implementation).** (1) The set ships as `Count`, `Sum`, `HighWater`, `LowWater` — bare `Max<F>`/`Min<F>` are deliberately NOT shipped: in a `u64` accumulator a genuine un-extreme unfold needs a second structure to restore the previous extreme, which is exactly the ceremony the presets exist to retire; the HighWater/LowWater names state the real semantics (both unfold as a no-op — the watermark contract). (2) The no-group mode: an ABSENT group block (`#[ok_reduce(Count)]`) declares a whole-table single group whose entry key is `[ns 2B][slot 2B]` with no group segment. (3) `LowWater`'s accumulator is `LowAcc` (a newtype whose `Default` is the identity `u64::MAX`): the typed read-modify-write seeds an acc with `Default::default()`, so the identity must live in the type, not a fold-time special case.

## Context

Reduce is okm's cross-document precomputation surface (ADR-0008): a user-implemented `ReduceLogic` (Acc + fold/unfold) driven by a `#[ok_reduce(Logic { group(..) })]` declaration, the fold hook running inside the write path's engine-locked read-modify-write. The mechanism is deliberately declarative — semantics the user supplies, machinery the framework supplies — and the write-path cost is paid only by tables that declare a reduce.

The contract works, but every real usage starts the same way: the user hand-writes a `ReduceLogic` impl whose fold/unfold is one of a handful of standard shapes — count the rows, track a maximum, sum a field. Four observations from the first production consumer (aura's state table, whose id assignment rides a MAX reduce):

1. **The boilerplate dominates.** A `MaxInstanceId` needs: a unit struct, an `impl ReduceLogic` (Document/Acc/fold/unfold), a payload mirror field so the reduce hook can see the folded value, and the watermark read via `reduce_get`. The aggregation semantics are two characters (`max`); the ceremony is fifteen lines.
2. **The common shapes are already constrained.** Count/Max/Min/Sum over a payload field are exactly the aggregates the reduce contract's reversibility clause names as safe ("count, sum, min/max qualify; median/distinct do not"). There is no user freedom for the framework to preserve here — only repetition.
3. **Max has a subtlety worth encoding once.** `unfold` for max must be a no-op when the folded quantity is an assigned identifier (ids never reused; the watermark must not fall), but a true un-max when it is a value that can shrink (a deleted row's maximum SHOULD leave). Both are correct; which one applies is a property of the FIELD's meaning, not something every user should have to reason out from the fold/unfold contract.
4. **Bindings will want them too.** ADR-0022 gives host-language callables the ability to implement `ReduceLogic`. A preset library gives bindings a byte-exact reference for the standard accumulators (`u64` BE and friends) — the dynamic side can declare `count()` without re-deriving the codec.

## Decision

**okm-core ships a preset combinator library next to `ReduceLogic`: standard accumulators as one-line declarations, zero boilerplate, same write-path mechanics.**

- **`Count`** — rows per group. Acc `u64`; fold +1; unfold −1.
- **`Max<F>` / `Min<F>`** — extreme of one numeric payload field per group. Acc `u64`; fold max/min into the accumulator.
- **`Sum<F>`** — total of one numeric payload field per group. Acc `u64`; fold add; unfold subtract.
- Declaration rides the existing attribute: `#[ok_reduce(Count { group(user_id) })]` / `#[ok_reduce(Max::<F> { group(type_id) })]` — the derive resolves the combinator to its Reduce impl exactly as it does a user-written logic type. No new attribute, no new wire format, no new slot rules (slots continue from the declaration-order counter, ADR-0016).
- **Idempotent-identity variants are separate names, not a flag**: `HighWater<F>` (unfold = no-op — for assigned-identifier watermarks) is a distinct combinator from `Max<F>`. The unfold ambiguity is resolved by NAME at the declaration site, visible in the schema, not by a boolean buried in a type argument.
- Combinators live in `okm_core` (`model/reduce.rs` or a sibling module) — they are library code with the same `VirtualStorage`-free, engine-agnostic discipline as `ReduceLogic` itself. No derive changes beyond accepting the names.

### What is NOT decided here

- **No built-in table statistics.** Row count, per-table max/min are NOT added to the engine: they would tax every table's write path for a capability most tables never read, and "which aggregate" is a user decision the framework has no business making. The line: mechanisms that are definitionally present (nothing today — even row count is a choice) stay out; user-selected semantics arrive by declaration.
- **`row_count()` convenience stays a reduce away.** A table that wants a row count declares `Count` in one line. If read frequency someday justifies zero-declaration counts, that is a separate ruling with its own write-path pricing — this ADR does not presuppose it.

## Honest semantic cost

- **`Sum` is not order-independent under overwrite** in floating point; the preset is integer-only (`u64` payload fields). Float sums remain user-written `ReduceLogic` (where the user can pick a compensation strategy), not silently lossy presets.
- **`Max`/`Min` unfold is lossy by nature** (removing the current maximum cannot restore the previous one without a second structure). `Max<F>`/`Min<F>` therefore require the reversible-identity reading: the group's stored value is an upper/lower bound that may legitimately be below the true extreme of remaining rows after a delete. Consumers needing exactness use `HighWater` semantics or a user-written logic with a companion `Count` to detect staleness. This is the contract's existing reversibility clause made concrete, not a new concession — but presets make it easier to hit accidentally, hence this paragraph.
- **The mirror-field problem for keyed quantities is solved by ADR-0024** (reduce hooks receive the decoded key; GROUP may name key fields) — recorded there because it is a hook-contract change, not a combinator concern. Once 0024 lands, `HighWater<F>` aggregates key fields by name and the mirror pattern is retired.

## Consequences

- Standard aggregations shrink to one declaration line; the fold/unfold ceremony disappears for the common shapes.
- The reversibility and overflow semantics of the standard shapes are encoded once, reviewed once, and tested once (okm-core's reduce test matrix gains the preset cases) instead of re-derived per user.
- Bindings (ADR-0022) get a fixed, byte-documented accumulator set to expose declaratively.
- Implementation: the combinators are generic `ReduceLogic` impls + the derive accepting them by name; test coverage in `reduce_test.rs`; docs pass in INTEGRATION/MODELING. Nothing in the wire format, slot allocation, or engine contract changes.
