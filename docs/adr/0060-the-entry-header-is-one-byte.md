# 0060: The entry header is one byte, and the codec gets a crate

> **Languages:** [English](0060-the-entry-header-is-one-byte.md) (primary) · [中文](0060-the-entry-header-is-one-byte.zh-CN.md)

**Status:** Accepted (2026-11-11) — the header landed the same day in `okm-core/src/model/obj_segment.rs`, both directions, locked by hex tests; the vocabulary half of decision 5 landed with it (`okm-value`: `Id`, `Names`, `Vocabulary`), and the value and segment types still owe their move (PLAN, same date).
**Date:** 2026-11-11
**Depends on:** ADR-0012 (the value vocabulary), ADR-0042 (layout versions are stamps), ADR-0043 (no aliases), ADR-0056 §5/§6 (replay is the migration), ADR-0057 (the block table), ADR-0059 (crate boundaries follow the axis)

## Context

Layout v2 gave the dynamic segment a block table and left each leaf's entry as three fields: `id`, `type u8`, `len varint`. That is the format's whole per-field overhead, and it is where v2 lost ground: a flat segment is 3 bytes per field plus a 2-byte head and a count, against the inline form's 3 bytes per field — so the table's locality was paid for by exactly the bytes it saved nothing on. Measured, the table is 30.4% of a six-field, 8-byte-body segment and 15.9% of a hundred-field, 16-byte-body one.

Two facts make a smaller header possible.

- **`len` is the width of a number.** The dynamic segment is widthless by design: an integer's bytes are its minimal big-endian form, so `UInt(7)` is one body byte and `UInt(300)` is two. A reader cannot know where a value ends without a length, and for a number that length *is* its width.
- **Three types cannot vary.** `Bool` is one byte, `Null` is zero, `F64` is eight. Their `len` carries no information at all.

So the length and the type are competing for the same eight bits, and one byte can carry both as long as the body is short.

## Decision

**1. A leaf's entry header is one byte: `[composite:1][len:3][type:4]`.**

```text
entry  = id varint | head u8 | body
head   = bit 7      composite: this entry owns the next block
       | bits 6..4  len: 0..6 inline, 7 = escape (a `put_len` varint follows)
       | bits 3..0  ObjValueType (0 UInt, 1 F64, 2 Str, 3 Bytes, 4 Bool, 5 Null,
                                6 Array, 7 Obj, 8 Int) — the numbers do not move
```

- **No separate length for short bodies.** `len ≤ 6` is in the header; `len ≥ 7` writes code 7 and a `put_len` varint, which is byte-for-byte what v2's length field already cost. Nothing gets bigger.
- **The three fixed-width types use no length at all.** The writer sets the len bits to 0 and the reader ignores them; their length comes from the type, as it does today by declaration.
- **Composite entries do not change.** They carry no body and no length, so their head byte is exactly v2's type byte with bit 7 set (`0x86`, `0x87`).
- **The type nibble keeps its numbers.** The type byte was already append-only (ADR-0012): slots 0–8 are what they were, so no stored meaning moves. Four bits leave seven spare slots — enough for `u128`/`i128` later without another layout change, which is the one thing a width-carrying type tag would have bought.

**2. What that buys, measured on the same shapes as ADR-0057.**

| shape | v2 | v3 | why |
|---|---|---|---|
| body ≤ 6 bytes, 1-byte id | 3 bytes per entry | 2 bytes | the length fits in the head |
| any body, `Bool` / `Null` / `F64` | 3 bytes per entry | 2 bytes | the type *is* the length |
| body ≥ 7 bytes | 3 bytes per entry | 3 bytes | the escape costs exactly what the length field cost |
| 1 000 `Null` fields | 4 496 B | 3 496 B | one byte per field |
| 20 000 `Null` fields | 99 495 B, refused | 79 495 B, refused | both past the 16-bit table |

So the gain is exactly one byte per field whose body is six bytes or shorter, and one byte per field whose type is fixed-width, and nothing at all for anything larger. That makes the change a Pareto improvement over v2 — never worse — but it does **not** close the gap to the pre-0057 inline form for large bodies: that form paid 3 bytes per field and no head, so `3 + 3n` still loses to `3n`. The flat-segment win is real only where the values are small: `3 + 2n` beats `3n` once a document has more than three fields.

**3. Everything that made v2 safe is untouched.** Bit 7 stays structural, so a type a newer writer invented is still skippable without understanding it; the length is still stored rather than derived, so a truncated value is still detectable; the arithmetic that has to agree is the same (`toc_len` inside the file, leaf lengths summing to the value area, `blocks = composites + 1`, table ≤ 65 535 bytes).

**4. It is layout v3, and the cutover is a wipe.** No new `#[ok_layout]` number and no stamp of the segment's own: the declared payload shape does not move, and a number no reader checks is ceremony. A store holding v2 segments is not migrated and not detected — it is deleted. One consequence needs saying because the bytes cannot enforce it: a changelog directory is built to outlive generations and picks each segment's declaration by its own `layout_version` (ADR-0056 §5/§6), while the segment layout does not ride that number — so a log carried across this change would hand v2 segment bytes to a v3 decoder. The wipe covers the log directories, and a log that must survive is exported by the build that wrote it.

**5. The codec leaves `okm-core`, and the value vocabulary goes with it.** The segment codec (`encode` / `decode`) is used by more than one crate — `okm-core` itself (`document.rs`, `model/graph.rs`) and `okm-dynamic` (`graph.rs`, `plan_fields.rs`) — and its currency, `DynamicValue`, is consumed further out still (`okm-dynamic` re-exports it as `Value`; `okm-lake` and the bindings build on it). The rule this ADR adopts is the plain one: **a codec one crate uses is a module in that crate; a codec two or more crates use is a crate of its own.** It is also ADR-0059's axis test — the unit of change should equal the unit that changes, and value types plus layout versions move on their own schedule, not the store's.

The crate cannot be the codec alone: its interface type is `DynamicValue`, so a codec crate that reached back into `okm-core` for its own argument type would be a cycle. The new crate is the schema-free value, its vocabulary view, and the codec over both — `okm-value`: `DynamicValue` / `DynamicField` / `ValueMap` / `CodecError`, `encode` / `decode`, and the vocabulary seam described in decision 6. `okm-core` depends on it; nothing is re-exported under the old paths (ADR-0043's posture).

**6. The codec is stateful, and the vocabulary dies with the handle that owns the store.** The segment carries ids, so both directions of the field-name vocabulary cross the codec's boundary. Calling the thing an *encoder* was the error in the first draft of this decision: it owns a vocabulary, so it is a vocabulary, and a vocabulary has state. `okm_value::Vocabulary` holds both directions and stays otherwise ignorant of the store; encoding and decoding are its methods.

- **No registry, no store identity, no invalidation.** A cache that outlives its store is a bug — a wiped store restarts its ids at 1, and an old cache would answer with ids no row carries — so the fix is to make that impossible rather than to key around it: the vocabulary is one field on the handle that owns the store (`Collection`), born with it and dropped with it. Two handles on one store keep one each and stay correct because a miss probes (below); nothing needs a scope token or an engine identity.
- **The store door is a trait of four methods, and it carries data, never key layout.** `scan` (what the store already knows, for the warm path), `name_id` and `id_name` (the two misses), `allocate` (spend the cursor; write name → id, id → name and the bumped cursor into the caller's batch). `okm-core` implements it once, where namespaces and slots live; `okm-value` never learns what either is, and its tests use an in-memory door with no engine.
- **Lazy by default; `init` is the warm path.** A miss probes in both directions: found means adopt, absent means allocate (name → id) or answer "no such id" (id → name, an answer rather than an error). Because the miss probes, skipping the load costs probes and can never mint a second id. A caller that knows it is about to touch many names warms once, because one prefix scan beats N probes; a handle touching three fields should not pay for a thousand.
- **What okm keeps is one field, one trait implementation and one call after a commit lands.** Allocation is not evidence until its batch commits, so an id handed out for an uncommitted label stays not-landed and a refused write leaves nothing behind; `landed()` is called after a successful commit (the old `promote`). Every other rule — the pending window, the mirror discipline, which direction to probe — lives in the crate.
- **The shape this crate starts with is measured, and it is not a trie.** `id -> name` needs no structure: ids are dense from 1, so it is a `Vec` indexed by id — a bounds check, no hashing, on the direction every decode walks. `name -> id` is a hash map with a keyed fast hasher over names borrowed from that same `Vec` (one copy of each name, nothing allocated on a hit). Measured at 100 and 1 000 names, per lookup: fast hasher 5–12 ns, SipHash 10–19 ns, a byte-wise trie 20–63 ns, a sorted `Vec` with binary search 54–69 ns. The trie only pays when the question is a prefix *range*, which is path-index territory (ADR-0029).
- **The struct door stays vocabulary-free** (ADR-0035): `get` / `put` never load it, so a vocabulary is created on the first segment door and fills lazily from there. A handle that never touches the dynamic segment never pays for it.

**7. What this is not.** `okm-value` is a code home, not a specification and not an interchange format. The segment's bytes remain a function of `(value, vocabulary)` — field ids come from the owning table's vocabulary, allocated on first sight — so the format is not canonical across stores and is not offered as one. A standalone format would need names inline (losing the economy) or a shared mutable vocabulary (becoming a stateful protocol); that question is settled elsewhere and this crate does not reopen it.

## Rejected

**Width-carrying types (`u8`/`u16`/`u32`/`u64` as separate type codes).** It saves the same single byte for numbers whose magnitude exactly fills a power-of-two width — but it spends the bits the length needs: five bits of type leave two bits of length, so every body of three bytes or more (every string, every large integer) pays an escape, and the cost moves from numbers to strings. The one ability it adds over this decision — the declared width appearing on the wire — requires padding, so a `u64` holding 7 costs 8 body bytes against 3 today. It fails the Pareto test this ADR is built on.

**Implicit length for the three fixed-width types only, two bytes otherwise.** Saves one byte on `Bool`/`Null`/`F64` and nothing on numbers, for the same format generation. A third of the available gain at the same price.

**Pad every numeric to its declared width.** Measured loss on exactly the values that dominate metadata-shaped documents (a `u64` holding 7 becomes 8 body bytes). ADR-0057's widthless choice stands.

**Put the codec in `okm-wire`.** That crate owns the frame protocol — op tags, keys, values as opaque bytes — and has no value semantics at all. The value codec has no transport concern and is on the store path; the axes differ.

**Keep it in `okm-core` because it already sits in one file.** A file is not a boundary; the change cadence is. Keeping the codec there makes "one place to change" a habit, and the crate makes it a property.

**Offsets, fixed-width entries, a block directory.** Unchanged refusal (ADR-0057): there is no partial read — `VirtualStorage::get` returns the whole value — so an offset buys parse arithmetic that costs less than the bytes it adds.

## Consequences

- Existing stores and their changelog directories are deleted, not replayed (decision 4). This leads because it is the only irreversible consequence: nothing in the bytes separates a v2 segment from a v3 one, so the two builds must not share a store.
- `okm_core::model::obj_dynamic` and `okm_core::obj_segment` paths become `okm_value::…`: breaking for `okm-dynamic`, `okm-lake`, the bindings, and core's own call sites. `ValueMap` / `NameResolver` / `CodecError` travel with `DynamicValue`, so the trait implementations that take them move too.
- "A leaf entry is byte-identical to the old inline frame header" (ADR-0057 §6) stops being true; the internals page, its worked hex example and MODELING's grammar line follow.
- The decoder keeps one rule: bit 7 set means no length; otherwise len code 7 means a `put_len` varint follows; otherwise the code is the length. One escape branch is the whole new code path.
- An earlier one-byte header idea — a length-less fixed-width type set with widths in the type — is recorded here as rejected, so the next review does not re-derive it.

## Related records

ADR-0012 (the value vocabulary and its append-only type numbers), ADR-0042 (a layout change is a new stamp), ADR-0043 (no aliases under renamed paths), ADR-0056 §5/§6 (a log outlives its stores, and each segment names its declaration), ADR-0057 (the table this header sits in), ADR-0059 (crate boundaries and the dependency-closure check). The bytes are documented in [the dynamic segment page](../internals/dynamic-segment.md); this record is the decision, that page is the mechanism.