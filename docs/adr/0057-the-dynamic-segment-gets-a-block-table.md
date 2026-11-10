# 0057: The dynamic segment keeps its values, and gets a block table at the tail

> **Languages:** [English](0057-the-dynamic-segment-gets-a-block-table.md) (primary) · [中文](0057-the-dynamic-segment-gets-a-block-table.zh-CN.md)

**Status:** Accepted (2026-11-10) — landed the same day in `okm-core/src/model/obj_segment.rs`; the segment's earlier layout is no longer written or read. **Date:** 2026-11-10
**Depends on:** ADR-0012 (the dynamic segment and its vocabulary), ADR-0035 (the whole-document surface), ADR-0042 (layout versions), ADR-0045 (the segment is truth, so its bytes are not derived)

## Context

The dynamic segment (slot `0x01`) is the one payload in the store whose shape is runtime data, so it cannot lean on a schema for offsets. Today it is a flat run of n-TLV frames — `[id][type][len][body]`, `Obj`/`Array` bodies inline — written in name order, with no version byte.

Three facts decide what a replacement may be.

- **Nothing streams, and nothing needs to.** The writer builds the whole value in one `Vec` and hands it to one `put` (`slot1_into`). A layout that needs the whole thing before it can be emitted therefore costs no streaming property — there was none.
- **The walk is not the expensive half.** Reaching field *k* parses *k* frame headers of three to seven bytes each, and the values in between are skipped arithmetically (`pos += len`), never read. What costs is decoding values and resolving ids through the vocabulary.
- **The shape that hurts has headers scattered among values.** Many fields, or one large `Bytes` in the middle: each header read lands on its own page, and a reader that wants the last field pays for the whole span. A contiguous table fixes that, and the table alone is enough — no offsets have to be stored to get it.

## Decision

**1. Layout v2 — a value area, a block table, and a two-byte length at the end.**

```text
value  = toc_len u16 BE | blocks | values
values = leaf bodies, block by block: block 0's leaves, then block 1's, …
blocks = block*          (block 0 = the top level; children in breadth-first order)
block  = count varint | entry*
entry  = id varint | type u8 | len varint     (leaf; len is the body's length)
       | id varint | type u8                  (Obj / Array; bit 7 of type is set)
```

- `id` is the vocabulary id; `0` means "no name", which is exactly what an array element is (ids start at 1).
- Bit 7 of the type byte says "this entry owns the next block". It costs nothing — the type byte is already there — and it keeps every entry self-delimiting: a reader knows whether a `len` follows without understanding the type, so an unknown type stays skippable ("a newer writer stored something I cannot see") instead of refusing the segment.
- `toc_len` is the byte length of the block region, written first and fixed width. The writer reserves it, emits the table and the values behind it, and patches it before the `put`; fixed width is what makes the patch possible — a varint cannot change without moving everything after it. It is also what lets a reader go front to back — length, table, values — without ever asking how long the blob is.
- **The table sits before the values.** That is what makes the front-to-back read possible, and it is the shape an in-place edit would want later: the table's offset is fixed, so changing one value's length moves bytes inside the value region only. Nothing writes in place today — the store replaces whole values and no per-value slack is reserved — but this ordering does not nail that door shut.

**2. No offsets are stored; positions are sums.** A block's value base is the sum of the leaf lengths of every block before it, and inside a block a leaf's body follows the previous leaf's. The table is contiguous — a few dozen bytes — so that sum is arithmetic over bytes already in hand. An offset per leaf would save the sum and cost one varint per field plus a fixed-width field per composite; a contiguous table does not need the help.

**3. Breadth-first, and the order is part of the format.** The *j*-th composite entry met while walking the blocks owns the *j+1*-th block. That is what makes a stored offset unnecessary for nesting — and it makes the writer's queue order load-bearing exactly like the name order inside a block: get it wrong and a reader lands on the wrong block.

**4. Layout, not markers.** No magic, no version byte: the layout is declared outside the bytes — the table and its sidecar (ADR-0042), the same fact that lets the store skip format-1 segments entirely (ADR-0056 §7). What catches a bad table is arithmetic that has to agree: `toc_len` inside the file, every block's leaf lengths summing inside `values`, `count` entries present, and the composite count matching the block count.

**5. The writer becomes a builder.** `put_frame*` appends frames to a byte stream, which this layout cannot do: the value area and the table are two regions, and the table is only known once the values are. The replacement collects `(id, type, value)`, emits leaves into the value area block by block, queues composites, and `finish()` appends the blocks and `toc_len`. `FrameScratch` goes away with the inline bodies: a composite has no length and no body to buffer.

**6. What does not change, and what shrinks.** The vocabulary still resolves names to ids; entries inside a block are written in name order, so one map still encodes to one byte string; the entry still lives in slot `0x01` and is still one `put` of one blob. A leaf entry is byte-for-byte the old frame header (`id | type | len`), so a flat segment grows only by its block count and the two-byte tail. A composite entry loses its length (it has none), and an array loses the element count inside its body — the block's `count` is that number now.

## Rejected

**Offsets in the table (a leaf's `off`, a composite's `child_off`).** One varint per leaf plus a fixed four bytes per composite, to save a sum over a few dozen contiguous bytes — the same arithmetic today's reader already does when it skips a frame. The bytes are the whole reason this layout exists.

**Values first, table at the tail.** The value buffer could then be the output buffer, so joining the two would be a single append — one copy of the segment saved per write. Rejected: every reader would start from the last two bytes, the blob would be unparseable without knowing its total length, and a later in-place edit would move the table along with the values. The streaming edge is illusory as well: the store's `put` takes the whole value, so nothing can leave the writer before the table exists.

**Lengths by difference (no `len`).** Saves one or two bytes per leaf and buys a neighbour dependency plus a sentinel at the end of the value area; a length that disagrees with its bytes then becomes undetectable entry by entry.

**A table over the top level only, nesting left inline.** Cheaper to land, and it fixes the flat case, but a large nested object still separates its siblings — which is the case this ADR exists for.

**Fixed-width entries.** An O(1) ordinal lookup is not the goal: eleven bytes per field against three to seven would pay for a lookup no caller asks for.

**A trailing marker.** A two- or four-byte magic at the end of a KV value cannot discriminate formats (that fact is declared outside the value) and cannot catch corruption the length and boundary arithmetic already catches.

**Moving the cold region of the static payload to the same shape.** Its fields come from the schema, its only reader walks them in declaration order, and it has neither the field count nor the random access that justify a table. It stays `[tag][len][value]`.

## Consequences

- A reader parses the tail, walks blocks in order, and computes each block's base by summing leaf lengths — reaching a nested block means walking the blocks before it. The code is paid once, in the one place that owns the layout.
- The write path builds two buffers — the table and the value region — and joins them once before `put`: one copy of the segment per write, bounded by its size. The buffers exist either way; only the join is ordered.
- No in-place update: the store contract has no partial write (`put` replaces the whole value) and this layout reserves no holes for growth. Patching one field by position would be a new door, not a property of the format.
- A segment written by the previous layout is not read. Existing stores have to be rewritten — the segment is truth (ADR-0045), so the migration is a re-encode of every row that has one.
- Arrays keep their order by block order, not by an id: their entries carry `id = 0`, which is why ids start at 1.

## Related records

ADR-0012 (the dynamic segment and the field-name vocabulary), ADR-0035 (`fetch` / `merge` / `replace` / `purge` over both slots), ADR-0042 (layout versions as stamps), ADR-0045 (what the change log carries, and why the segment is truth), ADR-0056 §7 (the other container in this repo, and why its markers went).