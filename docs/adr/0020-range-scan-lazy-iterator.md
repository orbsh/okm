# ADR-0020: Range scan as the engine's one ordered-read primitive — lazy iteration at the trait boundary

Date: 2026-09-21
Status: Accepted (implementation landed in this decision's commit; Collection-level semantics and docs to follow in the same pass).

## Context

The engine contract had exactly one ordered-read shape: `scan_suffix(prefix) -> Vec<Vec<u8>>` — an equality-prefix materialized into a vector. Three consequences:

1. **Range predicates had no physical landing.** `1 < a < 100` on an indexed field could only be answered by scanning the whole index (or the whole table) and filtering in memory — the exact degradation the "prefix = physical WHERE" boundary record exists to prevent. The knowledge that byte order == value order was already built into every encoding (BE fixed-width; VarInt is prefix-monotonic by design), but the engine API could not exploit it: an interval is not a prefix.
2. **No early exit.** Even for prefix scans, a consumer that wants the first 10 hits pays for all of them: the Vec is fully materialized behind the trait boundary before the caller sees one item.
3. **The engines already had the capability.** fjall 3.1.10, redb 4.2.0 and slatedb 0.16 all natively support key-range iteration with lazy iterators (details below). The trait shape was the only thing hiding it.

### Engine API survey (2026-09-21, versions as consumed by this repo)

| engine | range API | bounds | iterator | ownership |
|---|---|---|---|---|
| fjall 3.1.10 | `Keyspace::range(R: RangeBounds)` / `prefix(p)` | full-key `RangeBounds`, both bounds optional | `Iter: Iterator<Item = Guard>` — lazy, DoubleEnded | **owned + 'static**: `Iter` holds a snapshot nonce internally; `Guard::into_inner()` yields owned `(UserKey, UserValue)` |
| redb 4.2.0 | `Table::range(RangeBounds)` / `range_owned(RangeBounds)` | full-key `RangeBounds` | `Range<'a>` borrows the read txn; `OwnedRange` is 'static (Arc'd transaction guard) | **owned + 'static via `range_owned`** — the guard keeps pages alive after the `ReadTransaction` handle drops |
| slatedb 0.16 | `Db::scan_prefix(prefix, subrange: ByteRangeBounds)` | prefix + subrange RELATIVE to the prefix (empty prefix = whole-keyspace range); `ByteRangeBounds` impls exist for all `Range<T>` shapes over `AsRef<[u8]>` | `DbIterator` — async `next().await`, lazy | **owned + 'static** (boxed internal iterators); sync adaptation needs an owned runtime handle (`Arc<Runtime>`) and one `block_on` per `next()` |
| wire/remote (ADR-0010) | one `OP_SCAN` frame | key segment carries the begin; **value segment was empty** | response buffers the whole answer | buffered by construction — a frame is a message, not a stream |

The survey settled two design questions before they were argued: every native iterator is owned and `'static`, so a boxed `dyn Iterator` trait method loses nothing (no lifetime games, no handle leaks); and the remote path cannot stream without a wire protocol change, so it must be allowed to degrade.

## Decision

**One primitive: `scan_range`. One streaming form: `scan_range_iter`.**

```rust
pub trait VirtualStorage {
    /// THE ordered-read primitive: full-key `[begin, end)` in byte
    /// order; `None` end = unbounded; begin inclusive.
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>>;

    /// The lazy form: full (key, value) pairs pulled on demand.
    /// Default = buffered degradation over `scan_range`; engines with
    /// owned native iterators override with the real thing.
    fn scan_range_iter(&self, begin: &[u8], end: Option<&[u8]>)
        -> ScanIter;
}
```

- **`scan_suffix(prefix)` becomes a default method over `scan_range`**, via a new `prefix_end(prefix) -> Option<Vec<u8>>` helper (increment the last byte with carry; `None` when the prefix is all `0xFF`). Prefix semantics ride on the range primitive instead of being a second physical operation. Engines MAY keep overriding `scan_suffix` when their native prefix scan wins (slatedb's `scan_prefix(prefix, subrange)` does).
- **The lazy form is a separate method, not a return-type change.** `scan_range` stays `Vec` (the buffered shape every existing consumer expects); `scan_range_iter` returns `ScanIter` — half-way abandonment (LIMIT, first-match, early drop) costs only the reads performed. Engines override `scan_range_iter` where the native iterator is lazy; the trait default materializes first (a borrow cannot span the trait boundary) and is documented as such.
- **The remote path degrades on purpose.** `RemoteStore` rides the SAME `OP_SCAN` frame: the value segment — empty for a prefix scan — carries `[0x01][end bytes]` when a finite end exists, `[0x00]` for unbounded. No wire change (the frame already had a value field; ADR-0010's "wire sees op tags only" holds). `scan_range_iter` on a remote engine buffers the response in the frame, as frames do. Streaming the remote path is a wire-protocol decision, deliberately NOT taken here.
- **Empty intervals are a legitimate answer.** slatedb panics on an empty `Range` (end <= begin); the adapters return empty instead — the guard lives in the adapter, not the caller.

- **Why not one method returning `Box<dyn Iterator>` for everything?** The buffered and lazy forms have different failure and cost models: a buffered scan is a snapshot-sized allocation with a fixed cost, a lazy scan is an open-ended cursor the caller may abandon (fjall's snapshot nonce, redb's transaction guard and slatedb's iterator each hold engine resources until dropped). Two names state which contract the caller wants; a single name would force every existing `scan_suffix` consumer to reason about resource lifetime it never asked about.

**`ScanIter`: an opaque enum, not an exposed engine type.** The lazy form cannot return `Box<dyn DoubleEndedIterator>` — the trait is not object-safe. The escape is a concrete named type in okm-core:

```rust
pub enum ScanIter {                       // variants visible only inside okm-core
    Fjall(fjall::Iter), Redb(redb::OwnedRange<..>),
    Slatedb(SlatedbIter), Buffered(..),   // + the trait default / remote arm
}
impl Iterator for ScanIter { type Item = (Vec<u8>, Vec<u8>); }
impl DoubleEndedIterator for ScanIter { /* per-arm next_back */ }
```

Downstream sees only `next` / `next_back` / `rev` and can neither match nor depend on which engine iterates underneath. The enum is a Rust object-safety workaround (there is no `dyn DoubleEndedIterator`), and the enum itself is exactly what keeps it from leaking: adding an engine arm is an okm-core-internal event, invisible downstream. Reverse order: fjall and redb iterate backwards NATIVELY (verified in the API survey below — fjall `Iter` and redb `OwnedRange` both implement `DoubleEndedIterator`). slatedb's async `DbIterator` is forward-only; its `next_back` materializes the remaining tail once and drains it reversed — laziness lost where the engine cannot give it back, semantics identical.

**Why not make prefix the primary and encode ranges as prefix tricks?** A range's end bound is not generally a prefix of anything (`a < 100` ends mid-byte-space); synthesizing it would mean byte-level end arithmetic at every call site. The engine primitive is the range; the prefix is the derived convenience — matching what every backend implements natively.

## Collection level

`Collection::scan_range<I>(begin, end)` splices the encoded bounds between the entry header and the identity tail — the same region `entry_prefix` fills with an equality prefix — so a `1 < a < 100` predicate on the leading index field becomes a physical key interval. `Collection::scan_range_iter<I>` is the lazy mirror. The two names stay separate (matching the trait): `scan` = equality-prefix semantics, `scan_range`/`scan_range_iter` = the caller owns bound encoding. Leftmost-prefix composition carries over unchanged: on a `(a, b)` index, a range on `a` with `b` unprefixed is the widened form of the existing prefix discipline.

## Consequences and boundaries

- Every OKM encoding must keep byte order == value order for `scan_range` to be meaningful. BE fixed-width and prefix-monotonic VarInt hold this today; any future field type that violates it is automatically excluded from range predicates (and should be rejected at declaration time).
- The remote path's `scan_range_iter` is buffered until the wire gains a streaming frame — recorded as future work, not an obligation.
- `VirtualStorageAsync` gains the same pair in async shape (`scan_range_iter` there is a future item; the async junction surface is small enough that buffering is harmless today).
- **`Reverse<T>` loses its monopoly on descending reads, not its validity.** A `Reverse<u64>` timestamp bakes descending order into the KEY — entries physically sort newest-first, which benefits every reader of every scan shape and stays required where layout must express recency. `scan_range_iter(..).rev()` expresses the same read at QUERY time with an unwrapped field: zero wire change, direction chosen per query. Both stay available; the rule of choice is layout (many queries, fixed direction, worth paying in every entry) vs query (occasional or direction-varying reverse reads). `Reverse<T>` remains in the vocabulary unchanged — it is simply no longer the ONLY way to read newest-first.
- LIMIT-style consumers are the motivating case, but nothing in this ADR adds a query planner or predicate pushdown: bounds are bytes, the caller encodes them, `Collection::scan_range` is a convenience over the same seam as `scan`.
