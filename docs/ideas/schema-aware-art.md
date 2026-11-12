# A schema-aware ART for index entries

An outside proposal (2026-11-12), recorded as an idea rather than a design: replace the byte-at-a-time adaptive radix tree a generic KV would use with a **hybrid tree that reads OKM's compile-time key layout**. The claim is that most of what an ART does at run time — split, merge, optimistic path compression with a fallback re-read — is work paid for not knowing what a key looks like, and OKM *does* know.

## What the proposal says

A generic ART treats the key as an opaque byte string, so it must:

- split a compressed node when two keys diverge mid-path, then rebuild the suffixes;
- merge when a branch drops to one child, so the tree does not degenerate;
- keep only a bounded optimistic prefix (8 bytes is the classic figure) and, on a long compressed path, jump blind and then re-read the full key to verify no byte in between differed.

The proposed shape removes each of those in proportion to how much the layout is known:

1. **The namespace is a compile-time constant**, so it becomes a top-level array or hash dispatch instead of the tree's first two levels; nothing splits there, and those bytes need not be stored.
2. **Fixed-width segments become one step each**, not one step per byte: an `u32` field matched in full is a single integer comparison, so an `org_id` costs one level instead of four.
3. **Inline prefixes get an exact size** per segment instead of a bounded optimistic one, which removes the blind jump and therefore the fallback verification.
4. **Leaves need no key at all** when the key is fixed-width: the root-to-leaf path reproduces every byte of `KEY_LEN`, so a leaf can hold a value offset and nothing else.

The result would be a hybrid: static routing over the high, low-cardinality segments, and a reduced ART over the low, high-cardinality ones (the trailing primary key, timestamps, variable text).

## What OKM already guarantees

- The entry layout is a header (namespace, then a one-byte slot family since ADR-0060) followed by a segment of index fields and a carried primary-key tail. `segment_start` measures where the segment begins rather than assuming it, and `key_prefix_width` gives the tail's width.
- Index-field widths are known per access method, which is what makes `scan_range` bounds byte positions.
- The namespace is fixed per document, and slot families (`0x5nnn` leads, index tables, meta) are assigned at declaration time.

## Where the sketch does not hold

- **A key is not always fixed-width.** A trailing `Str` field is variable-width (bare UTF-8, no frame), and truncated `key(...)` prefixes carry only part of the primary key — that is exactly why `PrefixKey` has `taken < KEY_LEN`. For those entries the leaf cannot reconstruct the key by path alone, and the fourth claim (leaves store no key) fails.
- **Ordering is the engine's job today.** fjall, redb and slatedb already keep entries ordered and seekable; `ScanIter::seek` lands on the same snapshot or transaction rather than reopening. A custom tree would be a new storage structure — an engine — not a feature of the document layer, so the comparison is "replace or sit in front of the KV", not "improve a scan".
- The claim about cardinality ("the namespace never splits mid-way", "high-cardinality keys diverge in the first bytes") is a property of *this* schema family, not a theorem; a low-cardinality trailing field or a shared vendor prefix inverts it.

## What it would cost

An in-process index would have to own what the KV currently owns: durability, recovery, concurrency, compaction, snapshot semantics, and the seek contract the operators now depend on. The plausible middle ground — a schema-aware **memtable** index in front of an ordered engine — still needs a story for what happens on a cold start, and for keeping two orderings (its own and the engine's) from disagreeing.

## What to measure before believing any of it

- Entry counts and key widths per index, and how many entries share a namespace and a leading segment (the split rate a generic ART would actually pay).
- The share of indexes whose trailing segment is variable-width — that number decides whether the leaf claim applies to most of the corpus or to a corner of it.
- Whether the current cost is even in the tree: a scan is a range over the KV, and the operator layer's cost has been dominated by fetches and reopens, not by tree depth.

## Relation to the rest

Same status as the other outside proposals recorded here: not designed, not scheduled. The cheap half — static namespace dispatch and segment-width stepping — is only visible once something above the KV does the routing, which nothing does today.