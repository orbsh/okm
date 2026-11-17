# 0070 — A replacement is one batch, and the cut is the caller's to state

Status: decided 2026-11-17, implemented in both forms (`okm-core`, `okm-dynamic`).

## Context

Three shapes are one operation: merging two entities, splitting one, and swapping the subgraph between a set of entities that stay. Each is the same claim — "this shape becomes that one" — and each is expressible as retracts plus declarations.

Two things stopped that from being expressible at all:

- **The two verbs refuse each other.** `incorporate` refuses a fragment carrying deletions (`IncorporateWithDeletes`), `retract` refuses one carrying declarations (`RetractWithWrites`). Composing them is two commits, and between the commits the old shape is gone while the new one is not there: an entity's edges point at an endpoint that no longer exists. That is the state a replacement exists to make unreachable, and no ordering of two commits avoids it.
- **A moved assertion cannot keep its payload.** An edge's head IS its identity (ADR-0067), so moving one end is a different row. Rewriting it as `delete_edge` plus a declaration lands the payload type's **default**, because a declaration names a head and the creation path feeds the conflict hook `(default, default)` — it never sees the row being replaced.

The first is a transaction question and the engine already answers it: one `commit_batch` is one atomic write batch on fjall. The second is a read inside that batch, which okm can do and a caller cannot.

## The decision

1. **`Graph::replace(fragment)`** takes one fragment that both retracts and declares, and lands it as a single state change. The halves are planned **together**: retract's removals seed the write half's cluster plan, so every touched cluster is read once, counted once and written once. Running the two halves as two passes over one batch would make the second pass read the pre-batch row and re-stamp a count that ignores the first half's rows — silent corruption, not a performance note.
2. **`Fragment::repoint(from, to)`** moves an assertion to a new head with its payload. Free target: the payload lands as it was. Live target: `on_edge_conflict(stored at 'to', moved)` decides, the moved assertion being the declared side. The open attributes (the dynamic segment keyed by the edge) are **not** okm's and stay the caller's.
3. **`Graph::cut(NodeSet)`** is asked of a selection the caller already holds — it is not "read the subgraph", the selection IS the subgraph — and answers what a rewrite needs: one half-edge per edge crossing the boundary (the member end, which is the end re-attached, plus the far end) and the edges that die inside. Cost is the selection's degree, and an interior edge is deduped by its head because both ends reach it.
4. **Completeness is enforced.** A replacement that retires an entity must state every edge incident to it — repoint or delete — or the batch is refused (`ReplaceIncomplete`). The cascade is the right answer for a deletion ("the assertions are gone anyway") and the wrong answer for a replacement ("a connection nobody decided about").
5. **The older verbs keep one meaning each**: `incorporate` counts a repoint as a deletion and refuses; `retract` counts it as a write and refuses. Only `replace` takes one.

## What follows from it

- A selection at the **end of a chain** has exactly one half-edge on its boundary and a **detached** one has none. Both are ordinary: the first has one end to re-attach, the second nothing to re-attach. An edge between two members of a merged pair becomes a self-loop, and it is incident to the retired entity, so the completeness rule already forces the caller to delete it.
- Replacing **only the edges between entities that stay** is the same verb with no node deletion, so the completeness rule never fires. It is also why an edge's own deletion never touches its endpoints.
- The one thing okm will not decide is which half of a split a single boundary half-edge belongs to. That is judgement, and there is no basis for it in the store.
- **Nothing here is derived.** A replacement runs no reduce and invents no counter; what the fragment declares is what lands.
- **No id → id alias face**, though it was considered: a redirect asserts "this number is now that entity", and after a split, a partial re-merge and a later absorption that sentence is false. Silent redirection of history is worse than a missing row — the store keeps what is currently true, and the record of why a shape changed is application data.
