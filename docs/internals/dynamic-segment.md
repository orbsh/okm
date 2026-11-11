# Dynamic segment: the block table and the value area

Mechanism/implementation document for the value stored under slot `0x01` — the bytes at rest, the ordering contract a reader depends on, the size envelope the shape is built for, and what the same value costs once it crosses a serde boundary. The decision and everything it refused are in [ADR-0057](../adr/0057-the-dynamic-segment-gets-a-block-table.md); the field-name vocabulary and the value types are in ADR-0012; where the key sits is [key layout](key-layout.md). 中文版：[dynamic-segment.zh-CN.md](dynamic-segment.zh-CN.md). An entry's head is ONE byte ([ADR-0060](../adr/0060-the-entry-header-is-one-byte.md)): the length lives in the type byte for short bodies, and the three fixed-width types carry no length at all. A store written before this change is wiped, not migrated — nothing in the bytes distinguishes the two layouts.

## Where it lives, and who writes it

`[partition?][ns 2B BE][0x01][pkey]` → one entry per obj; the value is the whole segment. An empty map writes no entry at all (a delete, never a zero-entry table), so "absent" and "empty" are one state (`document.rs::slot1_into` / `slot1_map`).

`okm_core::model::obj_segment` owns both directions (`encode` / `decode`); `obj_dynamic` keeps the currency (`DynamicValue`, `DynamicField`, `NameResolver`) and its `decode_named` / `decode_variants` are thin delegates. Every writer goes through `encode`: `document.rs::slot1_into`, `model/graph.rs` (edge attributes), `okm_dynamic::graph`, `okm_dynamic::plan_fields`.

## The value: three regions

```text
value  = toc_len u16 BE | blocks | values
values = leaf bodies, block by block: block 0's leaves, then block 1's, …
blocks = block*            (block 0 = the top level; children breadth-first)
block  = count varint | entry*
entry  = id varint | head u8 | body
head   = bit 7      composite: this entry owns the next block
       | bits 6..4  len: 0..6 inline, 7 = escape (a put_len varint follows)
       | bits 3..0  value type (0..8)
```

Head first (fixed width), then the table (contiguous), then the value area (leaf bodies only, no headers among them). That order is what lets a reader go front to back — length, table, values — without ever asking how long the blob is.

## The head

Two bytes big-endian: the byte length of the block region. The writer reserves zeros, emits the table and the values behind it, and patches the head before the `put`; a fixed width is what makes that patch possible, since a varint cannot grow without moving everything after it. It is also the format's ceiling: 16 bits, so the table tops out at 65 535 bytes and `encode` refuses a segment that will not fit (`Error::Corrupt`) — measured at 20 000 `Null` fields, whose table is 99 495 bytes.

## A block

`count varint`, then exactly `count` entries — the member count of that object (or array; an array body no longer carries its own element count, this number is it). A block's end needs no length of its own: every entry self-delimits, so reading `count` entries lands on the next block's first byte.

## An entry

A leaf is `id | head | body`, one head byte carrying three fields at once. A composite is the same two bytes with bit 7 set: no length (a composite has no body) and no child offset (order decides, below).

The id lane is `u8` while `< 0xFF`, otherwise `0xFF` + `u16 BE`. `0` means "no name" and is what an array element carries (vocabulary ids start at 1). This is unchanged from the inline-frame era, which is why a leaf entry is byte-identical to the old `[id][type][len]` header.

The head's low nibble is `ObjValueType`: 0 `UInt`, 1 `F64`, 2 `Str`, 3 `Bytes`, 4 `Bool`, 5 `Null`, 6 `Array`, 7 `Obj`, 8 `Int`. Bit 7 (`0x80`) means "this entry owns the next block". That bit costs nothing — the head exists anyway — and it is what keeps every entry skippable without understanding its type: a reader knows whether a length follows, so a type a newer writer invented is skipped rather than refusing the whole segment. The middle three bits hold the body's length while it is 6 or less; code 7 says "a varint follows", which is byte-for-byte what a separate length field used to cost, so no body got bigger. And the three fixed-width types (`Bool` 1, `Null` 0, `F64` 8) spend no length at all: their middle bits are written 0 and ignored, because the type already answers.

`count` and `len` share `wire::put_len`, the prefix-monotonic varint every frame in okm uses: the first byte's leading ones give the width (1..9), `0xFF` escapes to 9. For lengths that is 1 byte under 128 and 2 bytes under 16 384; for counts, 1 byte under 128.

## A body

Minimal big-endian integers — unsigned drops leading zeros and is never empty; signed takes the shortest width whose sign bit still reproduces the value, so `-2` is one byte `0xFE`. `f64` is 8 bytes, `Str`/`Bytes` are raw, `Bool` is 1 byte, `Null` is zero bytes. An integer's width is not on the wire: a typed field's width comes from its declaration, and the reader widens into `U64`/`I64`.

## Order, which is part of the format

Inside a block, entries are written in the field NAME's byte order (every writer iterates a `BTreeMap`), and the vocabulary ids are allocated on that same walk — which is why one map at a fixed vocabulary encodes to one byte string. Blocks are breadth-first from block 0, and the *j*-th composite met while walking the blocks owns block *j+1*; that counter is what makes a stored offset unnecessary for nesting, and it puts both orders in the format rather than in a convention: a writer that queues differently, or a reader that numbers differently, lands a subtree under the wrong parent. The arithmetic checks catch the gross versions of that mistake, not the quiet one.

The value area is the leaves in table order, so a leaf's body begins at the sum of the leaf lengths before it — summed across blocks, not per block.

No offset is stored anywhere. A block's base is that same sum, and the table is a few dozen contiguous bytes, so the additions are cheaper than the field they would replace (ADR-0057 rejected an offset per leaf and a fixed-width entry table for that reason).

## Reading

1. `toc_len`, then `toc_end = 2 + toc_len`; the rest is the value area.
2. Walk blocks from offset 2: `count`, then `count` entries, accumulating `base` over leaf lengths and numbering each composite (`child = ++composites`).
3. Descend from block 0, following each composite's `child` block (depth cap `MAX_DEPTH = 64`, so a hostile value cannot walk the stack).
4. Resolve ids through the field-name vocabulary; an id the vocabulary has no entry for keeps its number as a string.

What a reader proves before answering: `toc_end` inside the file; every leaf's length summing to exactly the value area; `blocks.len() == composites + 1`; no truncated entry; depth ≤ 64. Any failure yields an empty field list rather than a guess — a torn or lying table reads as "no dynamic fields", and the document's declared half is untouched by it.

## What it costs

Write: two buffers (table, values) and one join before the `put` — one copy of the segment per write, bounded by its size. There is no in-place or partial update: `put` replaces the whole value and the format reserves no slack for growth, so `merge` is a whole-segment rewrite.

Read: the table is parsed on every decode (there is no projection door), bodies are copied into owned values, and each id is resolved once. In the document path name resolution dominates — `slot1_map` resolves every field through the vocabulary cache in `okm-value`, whose misses are engine probes — so the numbers below are the codec's floor, not the end-to-end cost of `fetch`.

Ceilings: table ≤ 65 535 bytes ⇒ 2 bytes per small leaf (id + head), 3 when the body escapes, 3–5 once the ids pass `0xFF`. That is tens of thousands of short fields but far fewer distinct names: a 20 000-name table is 79 495 bytes and `encode` refuses it. Depth ≤ 64 on read; the writer is queue-based and does not recurse.

## The size envelope (measured 2026-11-11)

`decode` in release, warm L2, no I/O, no name resolution, single field id per name; encode is not quoted because the vocabulary resolver sat in it.

| shape | total | table | table share | decode |
|---|---|---|---|---|
| 6 fields × 8 B | 69 B | 21 B | 30.4% | 210 ns |
| 6 fields × 64 B | 405 B | 21 B | 5.2% | 246 ns |
| 6 fields × 1 KiB | 6 171 B | 27 B | 0.4% | 479 ns |
| 6 fields × 16 KiB | 98 337 B | 33 B | 0.03% | 4 546 ns |
| 5 small + 32 KiB blob | 32 831 B | 23 B | 0.1% | 619 ns |
| 5 small + 1 MiB blob | 1 048 639 B | 23 B | 0.0% | 16 774 ns |
| 100 fields × 16 B | 1 903 B | 303 B | 15.9% | 2 751 ns |
| 1 000 fields × 16 B | 20 496 B | 4 496 B | 21.9% | 21 450 ns |
| 1 000 fields, `Null` | 3 496 B | 3 496 B | 100% | 7 800 ns |
| 5 000 fields, `Null` | 19 496 B | 19 496 B | 100% | 39 249 ns |
| 20 000 fields, `Null` | — | 79 495 B | — | `encode` refuses |
| object chain, depth 20 | 69 B | 65 B | 94.2% | 1 263 ns |
| object chain, depth 60 | 189 B | 185 B | 97.9% | 4 565 ns |
| 3 fields + nested 20 | 75 B | 50 B | 66.7% | 1 022 ns |
| 3 fields + nested 500 | 2 258 B | 1 509 B | 66.8% | 39 746 ns |

Three readings of that table:

- Overhead is 2 bytes per leaf (id + head) when the body is six bytes or shorter or the type is fixed-width, and 3 when the body escapes — so `~2/(b+2)` for small values and `~3/(b+3)` for large ones. That is why the 8-byte row did not move when the head shrank: 8 > 6 pays the escape, and the escape costs exactly what the separate length field cost. The gain is one byte per short-of-six field, never more.
- An entry costs about 8 ns to parse, read off the `Null` rows where the value area is empty (1 000 → 7.8 µs, 5 000 → 39.2 µs, linear). A body byte is cheaper by two orders of magnitude (20–50 GB/s in these buffers), so one field's table entry is worth roughly 300–450 bytes of body copy — that ratio, not the percentage, is what decides whether a shape belongs here.
- Locality is the whole point of the layout, and it only pays once the value span crosses a page. The shape the table exists for is small fields beside a body of several KiB or more, where reading a small field no longer touches the big one's pages at all — the constant factor the inline frames paid.

Where the shape is at its best: tens to a couple hundred fields, 64 bytes to 64 KiB per value, a total of a few KB to a few hundred KB, at least one body of page size. The extreme case is the target case — five small fields around a 1 MiB blob: a 23-byte table, one copy of the blob to decode, and every small field reachable inside those 23 bytes.

Where it runs out, in three directions:

- Tiny segments — a handful of fields, single-byte values, under ~200 bytes total. Nothing to buy: the inline-frame form was 3 bytes smaller and needed no per-entry list, so the table is a small regression at that size, not a win. That is a floor of the decision, not a defect in it.
- Thousands of fields. 1 000 fields decode in 22 µs (about what a 1 MiB blob costs) with a 22% table; 5 000 in 42 µs; 20 000 does not encode at all. The table has become the payload — that shape wants a second table, not a different layout inside this one.
- One value in the hundreds of KB to MB. The table is free by then; the cost is that every read materialises the whole value and every write re-encodes it. ADR-0036's answer is a second table on the same key type — moving the field out, not changing these bytes.

Deep chains sit in between: a 20-level object decodes in 1.2 µs but is 94% table, and its cost is one map per level rather than table walking — depth is bounded at 64 and cheap in bytes, so it is breadth that fills the table.

## Crossing a serde boundary

The block table never reaches serde: `DynamicValue`'s `Serialize`/`Deserialize` (feature `schema-serde`) see the owned tree that `decode` produced. What a caller pays is three things added together — decoding the segment into the tree, serde walking that tree, and the target format itself — and the layout's locality advantage ends at the decode, because serde walks map nodes and pointers, never the table.

Measured on the same shapes (release, JSON through `serde_json`; `decode` here includes building the caller's name-keyed map, which is the extra step a serde boundary needs):

| shape | segment | JSON | ratio | decode | to JSON | from JSON |
|---|---|---|---|---|---|---|
| object chain, depth 20 | 69 B | 149 B | 2.16× | 1 588 ns | 166 ns | 1 135 ns |
| 6 fields × 64 B | 405 B | 457 B | 1.13× | 467 ns | 171 ns | 301 ns |
| 6 fields × 1 KiB | 6 171 B | 6 217 B | 1.01× | 585 ns | 1 685 ns | 1 845 ns |
| 5 small + 32 KiB blob | 32 831 B | 131 184 B | 4.00× | 1 630 ns | 67 133 ns | 549 213 ns |
| 100 fields × 16 B | 1 903 B | 2 801 B | 1.47× | 12 846 ns | 1 338 ns | 10 037 ns |
| object chain, depth 20 | 70 B | 149 B | 2.13× | 1 588 ns | 166 ns | 1 135 ns |

Four readings:

- The cost tracks the value's bytes, not the layout. Ordinary scalars land at 1.0–1.5× the stored form; the 1.75× and 2.13× on the smallest shapes are format tax (keys, quotes, braces), not table tax. Both directions stay in the microsecond band — writing JSON is 58 ns to 1.7 µs, encoding the segment 0.5–0.6 µs — while parsing costs 2–4× the write, because it allocates the tree again.
- `Bytes` is the one cliff. JSON has no byte string, so `Bytes` serializes as an array of numbers (`0xAB` → `"171,"`), which turns 32 KiB into 131 KB (4×) and makes `from_json` take 549 µs — over 300× the segment decode, and 67 µs to write. That is JSON's property, not the layout's: CBOR and MessagePack keep a byte string and stay near 1×. The practical rule is that a document carrying a large `Bytes` does not cross JSON.
- Two collapses are unavoidable in JSON and documented on the impl: `Bytes` reads back as `Array`, and a non-negative `Int` reads back as `UInt`. Object keys must be strings, and `Deserialize` goes through `deserialize_any`, so the format must be self-describing — JSON, CBOR and MessagePack qualify; bincode and postcard do not.
- Turning the segment into a caller's tree is its own cost and scales with the field count: 100 fields × 16 B is 2.9 µs when `decode` hands back `Vec<DynamicField>` and 12.8 µs when it is keyed by name (one allocation per key). The document path pays one more step on top — every id resolved through the vocabulary cache, whose misses are engine probes — and that step usually costs more than serde does.

In practice: metadata-shaped documents (tens of fields, strings, integers, shallow nesting) cross JSON well — the output is the obvious `{"a":1}` form, microseconds, a bit over the stored bytes. A document with one big `Bytes` wants CBOR, MessagePack, or no JSON at all. And a caller who wants to serialize a SUBSET of the segment still pays for all of it: there is no projection door.

## What the layout deliberately does not have

No magic and no version byte: the layout is declared outside the bytes (ADR-0042, ADR-0056 §7), and a bad table is caught by arithmetic that has to agree — `toc_len` inside the file, the leaf lengths summing inside `values`, the block count matching the composite count. No offsets, no fixed-width entries, no in-place update, no block directory: each was weighed and refused in ADR-0057, and the one that most plausibly comes back (offsets, for O(1) depth navigation) should first be asked whether the lookup belongs to a path index (ADR-0029) instead. No reader for the previous layout either: a segment written by the inline frames is not decoded, so an existing store is migrated by re-encoding every row that has one — by replay through the new build's write path, each segment decoded under the declaration that wrote it (ADR-0056 §5) — not by a second decoder in the binary.

## Related

ADR-0057 (the decision and its refusals), ADR-0012 (the field-name vocabulary and the value types), [key layout](key-layout.md) (where slot 0x01 sits), [slot mechanism](slot-mechanism.md) (slot discipline, append-only), [MODELING: dynamic fields](../MODELING.md#dynamic-fields-the-document-api-adr-0012) (the user-facing surface), ADR-0029 (path indexes — the cross-document answer to "find by a nested path").