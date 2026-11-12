# Schema-aware index structures: a prefix tree, or a block index

Two outside proposals (2026-11-12), same premise, different level: OKM locks the key layout at compile time, so an index need not pay the generic "I do not know what a key looks like" tax. The first proposal applies that to an in-process prefix tree; the second applies it to the block index of a disk SSTable. Both are recorded as ideas, neither is designed or scheduled.

## The shared premise, and where it is weaker than it sounds

The premise holds where a key is fixed-width: the namespace is a constant, index-field widths are known per access method, and `segment_start` / `key_prefix_width` already measure the layout instead of assuming it.

It does not hold everywhere: a trailing `Str` field is variable-width (bare UTF-8, no frame), and a truncated `key(...)` prefix carries only part of the primary key — that is why `PrefixKey` has `taken < KEY_LEN`. For those entries the path no longer reproduces the key, and every claim below that depends on "the bytes are known from the address" weakens to "mostly known".

## Proposal A — a schema-aware prefix tree

A generic ART spends run time on split, merge, and optimistic path compression with a fallback re-read, all of which exist because it cannot see the key. A schema-aware variant would instead: route the namespace through a top-level array (compile-time constant, never splits, bytes never stored), step whole fixed-width segments at a time (`u32` = one comparison, not four levels), size inline prefixes exactly per segment (no blind jump, hence no fallback verification), and drop the key from leaves entirely where the path already reconstructs `KEY_LEN` bytes.

The honest objection is not the arithmetic, it is the ownership: ordering, durability, recovery, compaction, snapshots and the seek contract belong to the engine today (fjall / redb / slatedb). A prefix tree here is a *new engine*, not a feature of the document layer.

## Proposal B — a schema-aware block index

Same premise, one level lower and far cheaper to build, because it keeps the ordinary SSTable shape and only changes encodings:

- **Zero-parse binary search.** A generic index block stores `[key_len: varint]` before each key, so every probe decodes a length and then runs `memcmp`. With a fixed `KEY_LEN` the entries can be a packed array (key + offset) indexed by `base + mid * stride`, and the leading bytes can be compared as integers rather than byte loops.
- **A segment directory in the footer.** Instead of one flat index block interleaving every namespace, keep a small per-namespace (or namespace + leading field) offset table, so a lookup narrows the search window before the first probe.
- **A prefix Bloom filter.** The whole-key filter is useless for `scan_keys(prefix)`; a filter built over the leading bytes answers "does this namespace/tenant exist at all" without touching data blocks.
- **Key/value separation inside a block.** Store the shared leading segment once, the differing fixed-width suffixes as one tight array, values after them — so an in-block probe streams integers instead of dragging payloads through the cache, and becomes SIMD-friendly.
- **Shortest separators at block boundaries.** Truncate an index entry at the first byte where neighbours differ instead of copying the remaining fixed-width tail.

## Where the second proposal's own claims are too generous

- "Some of it is reachable by configuring fjall/RocksDB" is not true for the crates as used here: fjall's block layout, filters and separators are internal implementation, not knobs. Anything in this list is a fork or a new engine.
- A prefix Bloom filter needs a bounded prefix length; the moment a scan's prefix includes a variable-width field, the filter either over-approximates (it answers "maybe" for every longer prefix) or does not exist.
- The separation trick assumes the differing part is fixed-width and contiguous; a `Str` tail breaks both.

## Comparing the two

- Engineering cost: the block index reuses an SSTable's page discipline; the tree re-designs node encoding, splits and serialization.
- Memory: the tree folds prefixes best; the block index needs the segment directory plus separators to approach it, which is enough in practice.
- I/O shape: the tree risks cross-page pointer chasing; a block index reads one contiguous block, better locality by construction.
- Compatibility: neither is a configuration change — both mean owning storage, so the tree's disadvantage here is smaller than the sketch claims, and the block index's advantage is "less new code", not "no new code".

## What would make either worth doing

- Measure the corpus first: how many indexes have a variable-width trailing segment, how many entries share a namespace and leading segment, and how deep the current range scan actually goes. The operator layer's cost this session was in fetches and reopens, not in tree depth — that is the number to displace before rebuilding an index.

## Status

Same as the other outside proposals recorded here: not designed, not scheduled. The block-index route is the lower-risk one, and its first three items (fixed-stride search, segment directory, prefix filter) are each independent enough to land one at a time if an engine of our own ever exists.