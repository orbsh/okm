# ADR-0078: Attach attests a table once per store, not once per open

**Status**: decided 2026-11-21, landed the same day. No PLAN phase — this is a mechanism, not a
feature: two defaulted methods on `VirtualStorage`, an `AttachSet` on the engines that can answer
them, and a fast path at the top of `attach`. Measured before (1.38 µs) and after (0.17 µs).

`Collection::open` is attach (ADR-0074: one fallible constructor, the table is verified against
its store before the handle exists). What the verification does is point-read the meta row, decode
the stored declaration, compare it to the compiled one, and compare the capture flag. Measured on
a warm fjall store, that costs 1.38 µs per open; a point read on the same store costs 0.09 µs. So
the price is the comparison, not the I/O — and none of what the comparison protects can move under
a running binary.

## Context

Three facts decide where the answer to "has this store attested this table" can live.

The declaration is compiled in. `CollectionSchema::of::<K, R>()` is a pure function of the
declaration, and `R::PLAN` is filled once per process at a constant address. Nothing about a
running binary's declaration changes while it runs.

The row cannot change under a single writer. A fjall directory and a redb file take an exclusive
lock; an ephemeral store is per handle. The row a store wrote is the row attach will read, so a
verification that succeeded once in this process cannot be contradicted later by the same store.

But the claim is per store, and the plan is per ns. `__OKM_NS_{ns}_ENTRY` is keyed by the ns
number and shared by every store in the process (that is what `#[ok_shared_ns_in_tests]` exists
to bend). "This ns has been verified" is not: it is a fact about one store's row. Merging the two
— putting the attestation in the plan static — would make the second store of the same type look
attested and skip seeding its own row, and a table with no row is invisible to every reader that
has no declaration to compare against. Every test process opens many stores with the same types;
a service that reopens on a new directory does the same.

So the claim needs a key of (store, table header) — the header being `[partition][ns]`, because
two partitions are two tables — and the only question was whether that key lives in the store or
in a process-wide table of stores.

## Decision

The store answers. `VirtualStorage` gains two methods with defaults that preserve today's
behaviour exactly:

```rust
fn attached(&self, _header: &[u8], _decl: &'static str) -> bool { false }
fn mark_attached(&self, _header: &[u8], _decl: &'static str) {}
```

The key is the header *and* the declaration. The header alone was the first draft, and the
existing tests tore it: `attach_test` opens a `#[ok_shared_ns_in_tests]` twin on the same store,
which shares the header, and the twin's open went straight down the fast path and was never
compared — the refusal it exists to prove disappeared. The declaration is
`std::any::type_name::<R>()` rather than `TypeId::of::<R>()` because `Document` does not require
`'static`; the key only has to be unique inside one process.

`attach` asks first with both halves in hand. A hit warms the plan static (the one thing every commit reads, filled from
the declaration) and returns without a read, without a comparison, and without `note_table` — the
same store instance was told at its first attestation. A miss runs the full path unchanged and
calls `mark_attached` **last**, after the row is written and the store has been told, so a failed
attach leaves no claim behind.

Engines opt in by carrying an `Arc<AttachSet>` — one small vector of headers behind a mutex,
scanned linearly because a header is two to four bytes and a store has tens of tables, not
thousands. `Arc` because clones are one store (fjall's `Database`/`Keyspace`, redb's `Arc`, the
ephemeral store's inner `Arc`): a clone that attested must be seen by its siblings. `&S`
forwards. `ChangelogStore` does not add a set at all — it already keeps `header → Known` for
`note_table`, and the attestation is a hit in that map.

Stores that cannot answer keep the old behaviour: `RemoteStore` (a channel to a worker thread,
with no local view of the row) and any engine written outside this crate. `false` is always
correct; it only costs the read.

## What it bought

Same machine, release build, 20,000 opens on a warm store, before → after:

```
fjall open, warm        1.38 us  ->  0.17 us/op
ephemeral open, warm    1.66 us  ->  0.18 us/op
fjall get_row (miss)    0.09 us      0.10 us/op   (unchanged, for scale)
```

An open now costs about one point read instead of one point read plus a schema comparison. The
first open of a table on a store still pays the full path — 43.8 µs when the row has to be
written, 1.4 µs when it only has to be read — and that is the cost that belongs on the path that
makes a table exist.

## Rejected

**The attestation in the per-ns plan static.** The wrong scope: the static is shared by every
store in the process, so the second store would never seed its row. `OnceLock` → `LazyLock` does
not help — both answer "computed once per process", which is the bug, not the fix.

**Keying the claim by the header alone.** One ns, one table, one declaration — true in
production, false in the tests that keep the refusal honest: a `#[ok_shared_ns_in_tests]` twin
shares the header by construction, so a header-only key hands it the fast path and the comparison
that must refuse it never runs. A declaration that shares a header is a declaration that must be
compared, every time.

**A process-global map keyed by store identity.** It needs a stable id on every engine (a
counter, a pointer, a path), a story for the thousands of temporary stores a test process
creates, and it buys the same 1.2 µs as the store-local set. The lookup was never the cost:
scanning three bytes costs tens of nanoseconds against 1.4 µs.

**Making `Collection` `Sync` so callers hold one handle instead of opening per use.** That is the
other way to delete the per-open cost, and it is the wrong lever: the `Cell` behind the batch
counter is deliberate `!Sync` (ADR-0035 §10) — a merge's read-modify-write needs the exclusivity
that `&self` writes no longer get from `&mut self`. Replacing it with a lock moves contention onto
the write path to save a microsecond on open. A caller that wants to open once can keep the handle
per thread.

## Consequences

A second open of a table on a store no longer re-verifies anything. That is the point, and it is
safe exactly as long as a store has one writer; a store whose bytes another process rewrites
mid-life would keep reading the declaration it verified at first attach. That is out of scope for
every engine here (fjall and redb lock, ephemeral is per handle) and already true of `note_table`,
which caches the row it read.

A table whose layout was swapped under a store between two opens in the same process would not be
noticed by the second open. Nothing in okm can do that today: a layout change is a migration
(ADR-0075), which moves a table into its other parity and flips with a quiesce — the old generation
serves every read until then, and no handle is re-pointed in place.