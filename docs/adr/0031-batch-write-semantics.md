# ADR-0031: The batch write path carries semantics — a `BatchStore` adapter

**Status**: Accepted (2026-10-20) — implementation landed with this decision
(`BatchStore` in `okm-core/src/engine/storage.rs`, `Collection::save_with_prev_into`
in `okm-core/src/model/document.rs`; covered by `okm-core/tests/save_into_test.rs`)
**Date**: 2026-10-20
**Chinese**: [0031-batch-write-semantics.zh-CN.md](0031-batch-write-semantics.zh-CN.md)

## Context

`Collection::save_into(&mut batch, …)` is the cross-collection atomic path
(ADR-0003): several tables write into one externally owned `Batch` and one
`commit_batch` makes them live or die together. Its own contract says it is the
ENCODING surface, not the semantic one — it bypasses `put`'s higher write-path
hooks: the reduce fold/unfold, the embedded-document release, and the subscribe
emission. Two consequences follow, and both push work onto every batch writer:

- **A table that participates in a reduce cannot be written into a batch at
  all.** A reduce is a read-modify-write over a group entry (`apply_row` takes
  `&mut S where S: VirtualStorage`); a `Batch` is an operation collector with
  no `get`, so the RMW cannot see the current accumulator — and cannot see a
  value the SAME batch has already produced for the same group, which would make
  two writes in one batch each fold from the stale value and double-count.
- **An overwrite leaves the previous document's index entries dangling**
  (`put`'s documented contract: the entry key encodes the indexed value, so a
  changed value lands at a new key). A batch writer that overwrites must delete
  the old entries itself, or accumulate permanent garbage.

The consumer's only options today are therefore: abandon the batch (lose
atomicity) or re-implement the engine's ledger by hand. The second one is the
failure this ADR removes — a caller re-implementing id issuance, with its own
sequence table and its own recovery scan, is a second source of truth for a
ledger that already exists.

## Decision

**One adapter makes the batch a writer the semantic path can use.**

```rust
/// Reads see the batch's pending writes first, then the store; writes go into
/// the batch only. Point reads only — see the scan clause below.
pub struct BatchStore<'a, S: VirtualStorage> {
    store: &'a S,
    /// Behind a cell because `VirtualStorage`'s writes take `&self`
    /// (ADR-0026); the caller still owns the op list.
    batch: RefCell<&'a mut Batch>,
}
impl VirtualStorage for BatchStore<'_, S> { /* get / put / del, read-through */ }
```

1. **Read-through, in that order.** A `get` checks the batch's newest pending op
   for the key, then the store. A `del` on a key shadows any pending put for it,
   so a delete-then-put and a put-then-delete inside one batch both answer
   correctly. This is what makes a same-group fold see the value the batch
   itself produced.
2. **Scans are refused, loudly.** Merging a batch's pending ops with a
   store-level iterator is not a point read and is not needed by the reduce
   path. The scan methods return `Vec` with no error channel, so `BatchStore`
   panics with the contract rather than answering a partial view (every scan
   shape funnels through `scan_range`). A caller that needs a scan during a
   batch is modelling a different operation (read, decide, then open a batch).
3. **`Collection::save_with_prev_into(batch, key, doc, prev)`** — the semantic
   sibling of `save_into`: primary + faces (as today), `prev`'s faces that the
   new document no longer produces deleted, the embedded children `prev` held
   that the new document no longer does released (as `put` does), and the
   reduce fold/unfold applied through a `BatchStore`. `prev` is the caller's
   when it holds the old row (the common case: it just read it) and is read
   through the adapter when it is `None`.

   Its removal sibling is `Collection::purge_into(batch, key) -> bool`: the
   writes `purge` makes — primary, faces, the dynamic segment — into the caller's
   batch, with the same reads through the adapter, so a row this batch itself
   wrote is the state being removed. No subscribe emission, for the reason above:
   an uncommitted batch must not announce a write that may never land.
4. **One commit, one ledger change.** The reduce entries written through the
   adapter are ordinary batch ops, so a batch that is never committed leaves no
   accumulator change — the ledger's atomicity now matches the rows'.
5. **Subscribe emission stays out of the batch path** — declared absent, not
   silently dropped. A batch writer's events belong to it; emitting them from
   inside an uncommitted batch would announce writes that may never land.

**Vocabulary growth rides the batch, through the cursor.** Interning allocates an
id, and this ADR's scan refusal is what constrains how: the first cut derived
`max + 1` from a scan of the id face, which a batch cannot do, so
`save_with_prev_into` had to intern against the engine first. ADR-0032's cursor
removed the scan — allocation is now one point read of the id face's bare-prefix
singleton plus three point writes, all of which `BatchStore` serves, and the pending batch
answers its own cursor reads. So a batch writer's vocabulary growth, row and
faces commit together, and `save_with_prev_into` keeps `&self`.

## Consequences

- A caller with BOTH a multi-table atomic write and a reduce can now have both:
  id issuance (`HighWater`), counters (`Count`/`Sum`) and any hand-written
  `ReduceLogic` become declarable on tables written through batches.
- The hand-rolled alternatives retire where they are used this way: a private
  sequence table, its recovery scan, and the explicit "delete the old faces"
  code in the writer all collapse into one call. That is the point — the ledger
  and the index maintenance are the engine's, and the consumer states its
  schema.
- Single-writer boundary unchanged (ADR-0023 §"single writer"): the RMW still
  rides one serialized writer over one store handle; no CAS is introduced.
- The batch is invisible to readers until it commits, because reads do not
  consult it — throughput under concurrent readers is untouched.
- Batch writers emit no subscribe event, decided in ADR-0047: a batch is the
  bulk path, its consumers are other processes, and the change log already
  carries it with the guarantee the in-process face cannot give (staged before
  the commit, appended after it returned `Ok`). A caller that wants events for a
  batch turns the change log on. Still owed: a `save_into` deprecation path once
  `save_with_prev_into` covers every call site.