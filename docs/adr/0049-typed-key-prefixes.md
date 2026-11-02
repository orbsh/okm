# ADR-0049: a typed key prefix is a builder, and the prefix scans are lazy

Status: Accepted (2026-11-02).

## Context

A composite primary key already encodes a grouping: `[org][user][device]`
answers "every device of this user" with one prefix scan, and MODELING states
the reason — the key IS the query. But the only door onto it was one layer down:
reassemble `[partition?][ns][slot 0]` by hand, append the leading fields' BE
bytes, and call `store().scan_suffix`. That is a layer violation with a silent
failure mode (forget the partition escape and the scan reads nothing, not an
error).

The index side had a door but a narrow one: `scan` returns a `Vec`, so a LIMIT
could not stop the walk and a newest-first page had to materialize the whole
group before reversing it. `scan_range_iter` was lazy but speaks explicit
bounds — a prefix had to be translated into `[begin, prefix_end)` by the caller.

## Decision

1. **The probe is a type, not a byte slice.** `PrefixProbe<K>` carries the
   encoded leading fields and only a `Collection<S, K, R>` accepts it, so a
   probe cannot be spent on the wrong table or the wrong key.
2. **The probe is built by a type-state builder the derive emits**:
   `DeviceKey::prefix().org(7).user(11).finish()`. One zero-sized state type per
   depth, one field method per state, so declaration order is a compile-time
   rule. `finish()` is available at every depth — the empty probe constrains
   nothing and streams the table.
3. **`Collection::scan_keys(probe)` returns a lazy
   `DoubleEndedIterator<Item = Result<K>>`** over slot-0 rows, and
   `scan_document_keys(probe)` takes the same probe for the two-slot document
   mode (slot 0 ∪ slot 1, eager: the merge needs both sides in hand). A full
   scan is the EMPTY probe — `PrefixProbe::empty()` — not a second door, so the
   old zero-argument enumeration is gone rather than duplicated. Laziness is the
   interface on the row mode: `.take(n)` is a LIMIT that pays only the reads
   performed, `.skip(m).take(n)` is an offset page, `.rev()` walks the same
   window backwards.
4. **The index side gets the same shape, typed the same way.**
   `scan_prefix_iter::<I>` is the streaming twin of `scan`, and both (plus
   `scan_exact`) take a probe: either the derive-emitted `IndexProbe<I>`
   (`ByOrg::prefix().org_id(7).finish()`, one builder method per declared
   `fields(...)` entry, in declaration order) or raw encoded bytes for a probe
   assembled from data. The index marker type is the guard — an `org` probe
   cannot be spent on `by_label`. `ProbeBytes<I>` is the small trait that lets
   both pass through one door without a second generic parameter (a second
   parameter would break every `scan::<I>` turbofish). Prefix and range
   iterators share one private `entry_iter`, which owns its bounds — an opaque
   return type in edition 2024 captures every lifetime in scope, so borrowed
   bound bytes would have been captured too. `func(...)` indexes get no builder:
   their key bytes are the function's output, not fields.
5. **A length that does not match the declaration under a probe is reported**
   (`Error::Corrupt`), never skipped — the same stance `scan_keys` takes on a
   short primary entry.
6. **Rejected: one derived struct per prefix** (`X { a, b, c }` -> `X_ab`,
   `X_a`). It makes the key layout public API — every prefix name is a call-site
   name — and inserting a field between two others re-binds an existing prefix
   silently: same name, different bytes, no compiler complaint. Half of each
   type is also dead on arrival: a `Collection` is keyed by `K`, so a scan
   returns `K` / `PrefixKey<K>` and never the narrow type.
7. **Rejected: a name list** (`prefix_of(&[("a", 7)])`). It validates at run
   time what the declaration already knows at compile time, and an unknown or
   out-of-order name becomes a panic instead of an error at the call site.

## Consequences

- The builder type and its state types are emitted in the consumer's crate
  (an inherent `impl` on a foreign generic type is illegal), `#[doc(hidden)]`,
  and appear in no user-written signature except `prefix()`'s return type.
- `PrefixProbe::new` stays reachable for host-injected engines, documented as
  unchecked: bytes that do not match the layout scan the wrong window silently.
- The door answers prefix reads only. A dimension that is not a leading key
  segment is a key-layout question or an access-method question; the mirror
  workaround for such a segment retires when `#[ok_index]` can name a key
  segment, which this ADR does not attempt.