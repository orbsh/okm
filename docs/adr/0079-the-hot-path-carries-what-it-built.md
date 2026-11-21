# ADR-0079: The hot path carries what it already built

**Status**: decided 2026-11-21, landed as two commits the same day (`62c3cbf` for the header,
`24f80ef` for the plan and the entry buffer). No PLAN phase. ADR-0076 owns the third part of the
same idea — one `Entries` buffer, one producer — and is not re-claimed here.

A write on the typed path does three things before it touches the engine: it builds the key, it
finds the table's routing, and it builds the entries. Each of the three used to construct, on
every call, something that had not changed since the handle was made. None of them was a design
decision — each was the shape the code happened to have — and all three sit on the path every
write pays for.

## Context

**The key header.** `header()` answered `Vec<u8>`: the partition prefix (absent or `[0xFF][id]`)
followed by the ns, two bytes big-endian, assembled per call. A handle's ns cannot change after
construction — `with_ns` and `rebased` build another handle — so the answer is fixed for the life
of the handle, and yet every key built anywhere paid an allocation for it.

**The routing.** The plan (ADR-0076) says what a table's keys and entries look like, and the log
needs it per commit to decide whether a key belongs to a table it captures. Before the plan was a
static, the log answered that question by comparing headers it held — work per commit that grew
with the number of tables.

**The entries.** Covered by ADR-0076 §1: the plan became the single entry producer, appending into
one reused `Entries` buffer instead of returning owned pairs. What this ADR records is where that
plan lives, because the answer is what made the other two cheap.

## Decision

**1. A handle carries its header.** `[u8; 16]` and a length, filled once at construction by
`assemble_header` (the declaration's own `NS_PREFIX` verbatim when no override is set, so a
document that declares no ns keeps writing exactly the bytes it always did). `header()` answers
`&[u8]` into that buffer. The capacity is sixteen rather than the four a declaration can actually
emit (`[0xFF][id]` plus two) so a future prefix cannot silently truncate; the length is what every
caller sees.

Twenty-six call sites read the header and stop allocating; the eight that mutate it say `to_vec()`
and mean it. Warm-write allocator entries (`okm-core/tests/alloc_test.rs`, ceilings re-pinned):
typed put 18 → 15, merge 48 → 44, purge 22 → 19. `primary_key` also assembles its answer in one
buffer instead of building a three-byte header and growing past it. `put` on the ephemeral engine
with an index and a reduce: 1.3571 → 1.3081 µs.

**2. The plan is a static the declaration owns, and the log keeps it beside the segment metadata
it already held.** The derive emits, per ns, `static __OKM_NS_{ns}_ENTRY: OnceLock<TablePlan>`;
`Document::PLAN` is the `&'static` pointer to it; attach fills it once with `get_or_init` (a pure
function of the declaration). The log stores the reference in the record it keeps per known table,
so a table cannot be half-known — a header without its plan, or a plan without its directory. A
commit then asks `plan.carries_key(key)`: strip the header, test one bit. The cost of routing is
constant and does not grow with the number of tables in the store.

The static is emitted next to the declaration rather than inside generic code because a static in
a generic scope is one instance per instantiation — the storage has to sit where the declaration
does, and only a pointer crosses into `Collection<S, K, R>`.

That static is also the compile-time half of "one ns, one table": two declarations in one module
that claim the same ns define the name twice, and the compiler refuses it
(`the name __OKM_NS_7_ENTRY is defined multiple times`).

## Rejected

**Keeping `Vec<u8>` for the header.** An allocation per key on a path where the answer is fixed at
construction. The mutation sites are eight, and each of them is already copying bytes on purpose, so
`to_vec()` at those sites says more than a `Vec` returned everywhere.

**A `Vec` field on the handle instead of an inline array.** The allocation moves to construction
and stays a heap indirection on every key build. An inline array is free to copy and free to read.

**Compiling the plan per collection, or caching it per collection.** ADR-0076's rejection stands:
a cache reintroduces on the write path exactly the lookup the plan exists to remove. A static is
not a cache — it is one value per ns for the process, filled by the only code that knows how.

**Giving the log its own copy of the plan.** Two truths about one table, and the second one has to
be updated when the first is.

## Consequences

A handle's header is a value it holds, so a `Collection` clones its header with itself and no
caller can observe a header changing under it. `with_ns` and `rebased` produce handles that
assemble their own.

The plan's identity is the ns, so two declarations that share an ns share one static — which is why
`#[ok_shared_ns_in_tests]` suppresses the claim entirely, and why attach refuses a store stamped by
another declaration at the row instead of at the static.

Anything that needs a plan the declaration does not own — the dynamic form, whose declaration is
read from the store's meta row — still calls `TablePlan::compile` and holds the result itself.
The static is an optimisation for the typed path, not a second mechanism.