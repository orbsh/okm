# ADR-0055: A function index's result is an iterator

Date: 2026-11-06
Status: Accepted (implemented: `IndexFuncValues` deleted, `result_segments` added, both derive call sites switched, `[String; 1]` at every single-value declaration, array/iterator/empty coverage, `func_returns_a_plain_value` compilefail)

## Context

ADR-0005 introduced `func(path)` and said the return value "must implement `IndexFuncResult`" — one value, one entry. ADR-0015 noted in passing that a function returning `Vec<V>` fans out. Neither records the decision that grew between them: the mechanism accepts two shapes (a value, or a `Vec` of them) through a trait pair — `IndexFuncResult` for the encoding, `IndexFuncValues` for the shape — with the single-value arm written out as five separate impls, because a blanket `impl<T: IndexFuncResult> IndexFuncValues for T` overlaps the `Vec<V>` arm and Rust refuses overlapping impls.

That adapter is not free. It is a second trait to learn; a macro whose only job is to list five types; an error message that names a trait the reader has never met; and a coherence footnote in every explanation of the feature. The two shapes it adapts are the same shape. What a function index needs is not "a value or a Vec", it is "the values this row contributes" — and Rust already has that: `IntoIterator`.

## Decision

**1. One contract: the declared function returns something iterable of `IndexFuncResult`.**

```text
fn lower_name(row: &Doc) -> [String; 1] { [row.name.to_lowercase()] }   // one entry
fn tokens(row: &Doc)     -> Vec<String> { …split_whitespace… }          // N entries
fn buckets(row: &Row)    -> impl Iterator<Item = u64> { … }             // N entries, lazy
```

A single value is a one-element array — the shape that costs no allocation and reads as "exactly one". An empty iterator is zero entries, which is also how a function filters rows value by value. References encode as their values do (`impl<T: IndexFuncResult + ?Sized> IndexFuncResult for &T`), so a function can iterate what the row already holds — `row.ids.elems.iter()` — instead of cloning it into a temporary collection for the writer to walk.

**2. `IndexFuncValues` is deleted; `result_segments` replaces it.** A free function in `okm-core`, generic over `I: IntoIterator<Item = V>, V: IndexFuncResult`, producing one segment buffer per value. It is generic rather than a trait: nothing dispatches on the caller's type beyond what `IntoIterator` already does, so there is no arm to keep coherent with another arm.

**3. The generated call sites stop distinguishing shapes.** `entry_pairs` iterates `result_segments(#fpath(document))` and builds one entry per element, taking capacity from `size_hint` instead of a `Vec::len()` the mechanism no longer has; `encode_named` takes `.next()` — the first segment, which is the whole segment for a single-value result — instead of `.pop()`, whose "last element" was an accident of picking an arbitrary one.

**4. `IndexFuncResult` is unchanged**: `String` → raw UTF-8 bytes, `u8`/`u16`/`u32`/`u64` → big-endian. A value type outside those five implements it and nothing else.

## Alternatives

- **Vec-only** — the function returns `Vec<V>`, a single value is a one-element `Vec`. Uniform, and it deletes the same trait. Rejected: every single-value site pays a `vec![x]` allocation per put and a `vec![…]` wrapper in the signature, and the probe side acquires a question the mechanism cannot answer (which element is *the* probe?). The common case is one value per row; the noise and the allocation would land there to simplify the rare case.
- **Keep the two arms.** They work; their cost is the explanation. Rejected as a missed simplification, not as a bug.
- **`impl IndexFuncResult for Vec<T>`** — the elements would have to be framed to stay distinguishable, which sorts by length before bytes and destroys vocabulary order.

## Consequences

- Breaking, taken deliberately (no production data): a `func` returning `String` now fails to compile with `` `String` is not an iterator … required by a bound in `result_segments` ``, and the fix is `[String; 1]`. Locked by `tests/compilefail/func_returns_a_plain_value.rs`.
- The adapter, its macro and its coherence caveat are gone. A value type implements one trait; a multi-value result implements nothing — it just iterates.
- The write path loses one intermediate `Vec<Vec<u8>>` per put: segments are consumed as they are produced instead of being collected first and measured with `len()`.
- Probe semantics are unchanged — the query side calls the same path on its probe value and takes element 0 — but the mechanism stops pretending a multi-value result has a canonical probe. A function whose values are unordered is a modeling problem, not something the writer can resolve.
- `encode_named`'s "the first element is the segment" is now stated rather than accidental.
- The dynamic (bindings) path is untouched: `AccessMethodKind::Func` takes a callable returning **already-encoded segments** (`Vec<Vec<u8>>`), because a boxed closure crossing FFI has no element type to be generic over. It was already one form — bytes — and it keeps that form; the two paths differ in where encoding happens, not in how many shapes they accept.

## References

ADR-0005 (function indexes), ADR-0015 §4 (the `Vec` fan-out note), `okm-core/src/model/index.rs`, `okm-derive/src/document_encode.rs`, `docs/internals/func-index-mechanism.zh-CN.md`.