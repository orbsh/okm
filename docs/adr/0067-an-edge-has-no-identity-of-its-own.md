# 0067 — An edge has no identity of its own

Status: decided 2026-11-14, implemented. Supersedes the `edge_id` part of ADR-0017 §1 and narrows ADR-0066's member rows.

## The decision

An edge is identified by `(src, predicate, dst)`. There is no edge number: nothing to allocate, nothing to preserve across a restore, nothing for a caller to remember.

The identity is fully determined because every part of it is already fixed:

- `src` and `dst` are `NodeRef` = `[ns 2B][pkey]`, and every node collection has its own ns by rule (one ns per entity class, never reused) — the ns is the collection tag, so two collections cannot collide.
- The predicate is a vocabulary id (`Vocabulary::<u16>` behind the `0x04`/`0x05` faces), assigned by the declaration, not by the writer.
- Node keys are fixed-width integer encodings, not strings, so the whole tuple is a fixed shape.

## What this changes

`0x06` stops being an index that points at an edge and becomes the edge: `[ns][0x06][src][pred][dst] -> payload`, the same row that already had to exist for traversal. `0x07` stays the reverse face and stays empty. Cluster member rows and the edge-to-cluster index carry the tuple instead of a number. `Decl::edge_at`, the edge-number issuer, and the "preserve the number across a restore" rule all disappear.

Two things this buys beyond fewer concepts:

- Writing the same edge twice is writing the same row. Idempotence stops being a claim in the docs and becomes a property of the key.
- A cascade delete can no longer orphan an edge whose number was reused, because there are no numbers to reuse.

## The multiplicity question

Someone will ask how to record "the same pair, the same relation, five times" (five contracts, five clicks). The answer is that those are not five edges: the payload is a `Variant` (`okm-value/src/value.rs`), which holds an array, and the array is read, replaced and appended to atomically as one value. A conflict hook makes the append explicit: `on_edge_conflict(|old, new| { old.extend(new); old })`.

That is also why this ADR could not have been written before the hooks existed. With a mutator closure that only saw the stored row, multiplicity had nowhere to live except duplicated structure.

## Rejected: a compact graph-internal id

Considered and deferred, not refused. A dense `u64` per node, with `(ns, pkey) -> gid` and `gid -> (ns, pkey)` beside it, would make keys shorter: `NodeRef` is 10 bytes for a `u64` key versus 8, and an edge's tuple appears in two to four keys, so a node pays for its mapping rows inside two to three edges. Two rows go into the `Extension` band as subkinds, so no core slot is spent.

It loses on the read side. Today a hop decodes `far` out of the traversal entry and reads nothing; with `gid` in the key, resolving `far` back to a `NodeRef` is another point read per hop, or a leak of internal encoding into every API that returns an edge. It also gives nodes a lifecycle they currently do not have: an id must be issued when a node first appears as an endpoint and reclaimed when its last edge leaves, which is a resource to maintain where the model now has none.

Reopen when either is measured: node keys well above eight bytes (composite keys, keys carrying `Refs`), or degree high enough that the tuple repeats four times per node. Until then the two bytes of ns are not worth a mapping.

## Other ways to shorten a key, cheapest first

1. **ns as a set ordinal.** The `#[ok_nodes(..)]` list is a closed, compile-time known set, so a member table's ns can be written into edge keys as a one-byte ordinal instead of two bytes of ns. Saves two bytes per endpoint occurrence, costs nothing at run time, and keeps the key deterministic. It trades the "ns is the collection tag" property for "ordinal is the collection tag inside this graph", which is why it is not free: two graphs can no longer share a key layout by accident, and a reader needs the graph declaration to interpret an ordinal where it used to need nothing.
2. **Predicate width.** `u16` becomes `u8` only if the vocabulary is capped at 256 labels. Worth it only for a graph known to stay small; the cap is a declaration-time decision, not a runtime check.
3. **Nothing else, honestly.** Prefix compression in the storage engine already removes the repeat: rows under one `src` are adjacent, so their shared prefix is stored once per block. What remains expensive is the differing suffix and the comparison cost, not the disk bytes. Varint keys would save bytes and destroy the ordering the faces depend on, unless an order-preserving variant is used — at which point the saving is a byte or two on the widest keys only.

## Splitting a tuple that has variable-width segments

`[src][pred][dst]` with no edge number leaves a key whose first and last segment have widths the reader does not a priori know. Three encodings were weighed.

**Length in front** (what `NodeRef` already does: `[ns][len varint][pkey]`). Parse left to right: read one self-delimiting reference, skip the two predicate bytes, the remainder is `dst`; validation is "a strict parse must consume exactly the key". Cost: one or two bytes per hop, and ordering within a `(src, pred)` group is length-first, not value-first.

**Length in the tail** (`... [len_src][len_dst]`, fixed width, read from the end). Ordering starts with content, and adjacent rows share a longer prefix. Costs: the lengths must be fixed width (a per-key ceiling of 255 with one byte), range bounds have to carry the tail, and the "a key that prefixes another" case flips order — the shorter key's length byte is compared against the longer key's next content byte, so short-before-long only holds when the length happens to be smaller. Value ordering through a prefix relation is therefore not reliable.

**No length at all.** This is the one that matters, because it follows from the model rather than from an encoding trick: every collection's key is already fixed width (`KeyEncode` emits a fixed byte count per declaration), so a fixed-width identity makes both length fields redundant. `[ns][pkey][pred][ns][pkey]` parses by width alone, sorts by value, and costs nothing per hop.

The corollary is a constraint worth stating once: a table that wants to appear in a graph must have a fixed-width key. Variable-width fields (`Str`, byte strings, anything length-encoded) may still be indexed inside a collection; they are refused as graph member identities. Where a variable key genuinely must participate, the honest options are to pad it to a declared width (right-padding preserves lexicographic order if the pad byte sorts low) or to accept a hash identity and its collision check — not to smuggle a length into every key.

## Where the graph's own identity goes next: a boundary-only handle

The tuple identity is what the surface speaks. Internally the graph may carry its own handle: a dense `u64` per node, issued when a node first appears as an endpoint, resolved to `(ns, pkey)` only where a caller, a collection read or a delete needs the real reference. This is recorded now because it fixes the shape of the two faces it would use and nothing else.

Faces, all in the low core band, none in Extension (`0x10`, `0x11`):

    handle -> node      [ns][0x10][handle u64]   -> [node_ns 2B][pkey]
    node -> handle      [ns][0x11][node_ns][pkey] -> [handle u64]

Traversal faces keep the same counts and the same slots; only their interior changes: `[src_handle][pred][dst_handle]`, eighteen bytes regardless of how wide a member table's key is. Nothing else is stored in the row but the edge's payload — the opposite endpoint does not need to be duplicated into the value, because the handle is the graph's own identity and resolving it is what the handle face is for.

What it costs and what it buys, stated so the decision can be re-measured rather than re-argued:

- Space. A tuple costs `2 + key + 2 + key` bytes today. The ordinary case is a composite identity such as `(u32, u64, u32)` — sixteen bytes — which makes a tuple thirty-eight and a handle eighteen: less than half, four times per edge (`0x06`, `0x07` and the two predicate faces). A bare `u64` key gives twenty-two against eighteen, which is where the saving is small.
- Registry. Two rows per node, roughly fifty bytes together, paid once per node rather than per edge. A sixteen-byte composite key therefore breaks even at a degree below one — the first edge pays for its endpoints' rows — while a bare `u64` key needs a degree of three or four before it wins on space alone.
- Reads. A hop reads no row either way, because `far` is the handle and traversal stays in handles. A reference is resolved once per result, or in bulk: handles are issued monotonically, so a sorted set of results is one range scan over `0x10`.
- Writes. One handle lookup per unique endpoint in a fragment, plus the two rows on first sight.
- Lifecycle. A handle outlives its last edge only as waste; the alternative is reference counting, which is a resource the model does not otherwise have. Waste is bounded by the number of nodes that have ever had an edge, and a handle space of `2^64` makes it a space question, not a correctness one. Compaction, when it exists, reissues from zero.

Ordering is issue order, not key order: a range over handles is a range over "when this node first appeared", which no face may rely on for value ordering. That is why a handle can never serve as a key-order proxy, and why the graph's own faces never claim to be sorted by node key.
