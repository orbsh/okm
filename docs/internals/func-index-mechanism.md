# Function indexes: func, return shape, and multi-value fan-out

> **Languages:** [English](func-index-mechanism.md) (primary) · [中文](func-index-mechanism.zh-CN.md)

This is the mechanism and implementation document for function indexes (`func(path)`): how the declaration expands, the value encoding contract, how multi-value fan-out happens, and how one declaration drives both sides. For the general entry layout, scanning and fetch-back, see the [index mechanism](index-mechanism.md); for modeling discipline see the [modeling guide](../MODELING.md).

## Positioning

A function index answers this question: **the data segment is not the direct encoding of some field, but a derived value of a row**. Typical cases:

- Normalisation — `lower(name)` lands `Apple`/`APPLE`/`apple` in the same segment (case-insensitive index);
- Inverted index — `tokens(text)` splits one row into several tokens, one entry per token (full-text search, multi-value fields);
- Bucketing — `hour(created_at)` folds a timeline into fixed intervals.

`fields(...)` costs nothing (it encodes field bytes directly), while a function index pays one function call plus one allocation on every write — so it is the upgrade path for when fields cannot express the thing, not the default shape.

## Declaration expansion

```rust
#[ok_index(by_token { func(tokens) })]
```

The derive (`okm-derive/src/document_encode.rs`) generates the same marker struct `__OkmIndex_{Row}_{name}` for every func index; in the `KvIndex` impl the difference from a plain field index is exactly one place:

```text
const FUNC: &'static str = "tokens"    // empty string for plain indexes
```

FUNC is currently a **documentary constant** (it records the path name; a test asserts on it to lock the declaration); what actually drives writes is not this constant but the `entry_pairs` override below — the mechanism forks between func and plain indexes on the write side, not inside the constant.

## The value encoding contract: IndexFuncResult + IntoIterator

A function's return value is first encoded into segment bytes by `IndexFuncResult`, and the return shape then decides how many entries come out — the latter is the language trait `IntoIterator`, not a trait of this crate (`okm-core/src/model/index.rs`):

```text
IndexFuncResult      byte encoding of a single value (data-segment format)
  impl: String       → raw UTF-8 bytes (variable width)
  impl: u8..u64      → BE, fixed width

return shape         IntoIterator<Item = V>, V: IndexFuncResult (ADR-0055)
  single value       → one-element array [v] (on the stack, zero allocation)
  multiple values    → Vec<V> / slice / any iterator, one entry per element
  empty iterator     → 0 entries: the row produces no entry at all
  reference          → `&T` encodes like `T`, so a collection already in the row can be iterated without cloning
```

The return shape has exactly one contract: iterable. There is no adapter trait such as `IndexFuncValues`, hence no overlap between `impl<T: IndexFuncResult> IndexFuncValues for T` and a `Vec<V>` arm, and no macro that has to list every type for the single-value arm — one `IntoIterator` carries single and multiple values alike (ADR-0055). A single value written as `[v]` goes through a stack array, zero allocation; a multi-value `Vec`/slice/iterator is iterated as it is. Lazy sources lose nothing: the writer receives the iterator and writes entry by entry immediately, never collecting first.

**An empty iterator is one way to write a row-level filter** (the other is the `where(path)` predicate on the declaration, see the [index mechanism](index-mechanism.md)). The division of labour: `where` is a row-level condition (visible in the declaration, available to plain indexes too), while filtering inside the function body is per-value discard (drop one token of this row, keep the rest). Mixing a row-level condition into the value function degrades "one declaration drives both sides" into "the caller has to know which half inside the function must be mirrored on the probe" — that is the reason for choosing `where`.

The encoding follows the established conventions of field encoding: `String` as raw UTF-8 (variable width, lexicographic order = byte order), integers BE (fixed width, numeric order = byte order). A function index therefore inherits every constraint of the data segment automatically — at most one variable-width segment, and it must sit against the primary-key prefix (the func value is that one variable-width segment; the key prefix is inferred from the tail); a fixed-width field appended after a func value does not occur under the current derive (func and fields are exclusive, one declaration picks one).

## Write side: the entry_pairs override and fan-out

A plain field index uses the trait default of `KvIndex::entry_pairs` (one `(entry_key, entry_value)` pair). The derive generates an override for a func index (this is the only mechanism fork for func indexes):

```rust
fn entry_pairs(table_ns, key, row) -> Vec<(Vec<u8>, Vec<u8>)> {
    if !admits(row) { return Vec::new(); }      // where(path) predicate (always true without a declaration)
    let vals = result_segments(tokens(row));     // value → segment bytes, produced one by one
    for seg in vals {                            // one entry per element
        [ns 2B][slot 2B][seg][key prefix]        // key; the value is always empty (func and includes are exclusive)
    }
}
```

Points to note:

- **The entry value is always empty**: `func(...)` and `includes(...)` are exclusive (rejected at declaration time, by the field validation in `schema.rs`), so a function index has no covered segment — `Covered = ()`, and `scan_covered` never applies to it; each token of an inverted index carries only the primary key, not a materialised copy of the payload.
- `index_entries` (the actual put/delete entry point) calls `entry_pairs` for every declared index and flattens the result; it is unaware of fan-out: the write loop only sees "N pairs", and a plain index is the N=1 special case.
- `purge` uses the same function to produce the set to delete (one `del` per entry) — as long as func is pure, a write of N entries is a delete of N entries, and **purity is the contract**: reading external state inside func (clock, random number) makes delete produce a different set than the write did, leaving dangling entries.

## Read side: probe normalisation, no new scan path

The read side adds no mechanism — the token is the data segment, and `scan::<I>` works as usual:

```text
t.scan::<ByTag>(b"rust")
  1. entry_prefix = [ns 2B][slot 2B] + b"rust"
  2. range scan → length check (suffix.len() ≥ tail width) drops false hits → cut the primary key out of the tail (inferred from KEY_LEN) → fetch back
```

**Probe normalisation is the caller's obligation**: the query side feeds the probe value through the same func and encodes it (in the test, `lower_name(&probe)`), so one declaration drives both sides. The derive cannot do this for you — the probe is in the caller's hands, not a row. This means func's encoding must match the write side exactly (same function, same serialisation); consistency is guaranteed by "reuse the same function", which is precisely the payoff of writing the path into the declaration rather than hand-encoding at the call site.

Multi-value scanning has exactly the semantics of a plain leftmost prefix: `b"rust"` hits every row containing that token; whole-token exact matching is the caller's problem at the scan boundary (a variable-width segment has no terminator, so a prefix scan is inherently "starts with this", and exact matching can scan out the current token's interval — stopping when the next entry's token segment changes).

## Extension layering: why func is an fn, not a trait

The outward extension surface is three layers, each with its job:

```text
declarative (no extension point)   fields / includes / key      macro generates directly, user code never enters encoding
business derivation logic          func(path) — a plain fn      the only opening through which user code enters entry encoding
value encoding seam                IndexFuncResult              segment encoding of a new value type (encode/width)
return shape                       no extension point           IntoIterator is a language trait: arrays/Vec/slices/iterators all count
```

The correct division of extension work: a new value type only implements `IndexFuncResult` (one trait, no second adapter to align with); the return shape has no extension point — `IntoIterator` is a language trait, so a new fan-out shape means returning a new iterable type; business derivation logic is one fn. func is not a trait, for four reasons:

1. **Zero capability gain** — anything a trait can do, a fn path can do. If "several implementations of the same concept's func" are needed, two fns are two implementations; index declarations are compile-time, so `func(lower_v1)` / `func(lower_v2)` are directly selectable.
2. **A trait breaks "one declaration drives both sides"** — probe normalisation today is "call the same path"; with a trait the probe side would have to name the same impl, heavier caller syntax for an unchanged mechanism.
3. **Smallest seam** — the derive needs only "a callable path", and fn is the minimum that satisfies it; wrapping it in a trait imposes framework ceremony on business logic (write `fn tokens(&Doc) -> Vec<String>` and stop — that is the strength of the current design).
4. **The declaration is the registration** — the real motive behind trait-ification is usually making the framework aware of user logic (registration, discovery), whereas `#[ok_index]` is already the registration point; the function is merely the implementation pointed at, and no second layer of bookkeeping is needed.

## Divergence from fields, summarised

```text
             fields(...)                            func(path)
data segment source   payload field encoded directly            function return value encoded
write-side entry      trait default (1 pair)                    entry_pairs override (one pair per produced value)
row-level filtering   where(path)                               where(path), or the function body returns an empty iterator
compile-time checks   multi-field combinations, variable-width position checks  iterable, and the element type implements IndexFuncResult
probe                 caller encodes the probe value            caller calls the same path to normalise the probe
cost                  zero extra                                one function call per write + one segment buffer per value
```

A function index introduces no new entry layout, scan path or runtime bookkeeping — it only swaps where the data segment comes from, from a field read to a function call, and reuses everything else in the index mechanism.