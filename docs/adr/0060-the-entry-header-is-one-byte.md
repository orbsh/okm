# 0060: The entry header is one byte, and the codec gets a crate

> **Languages:** [English](0060-the-entry-header-is-one-byte.md) (primary) · [中文](0060-the-entry-header-is-one-byte.zh-CN.md)

**Status:** Accepted (2026-11-11) — decided, not landed. Layout v2 ([ADR-0057](0057-the-dynamic-segment-gets-a-block-table.md)) is what the code writes and reads today; the landing checklist is the PLAN section of the same date.
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

| shape | v2 | v3 | table share |
|---|---|---|---|
| 6 fields × 8 B | 69 B, table 21 B | 63 B, table 15 B | 30.4% → 23.8% |
| 100 fields × 16 B | 1 903 B, table 303 B | 1 803 B, table 203 B | 15.9% → 11.3% |
| body ≥ 7 bytes | `type` + `len varint` | `head` + `put_len` | unchanged |
| `Bool` / `Null` / `F64` | `type` + `len` | `head` only | −1 byte |

A flat segment becomes `2 + 1 + 2n` bytes against the inline form's `3n`, so v3 is *smaller* than the layout v2 replaced as soon as a document has more than three fields — the one regression v2 carried is gone.

**3. Everything that made v2 safe is untouched.** Bit 7 stays structural, so a type a newer writer invented is still skippable without understanding it; the length is still stored rather than derived, so a truncated value is still detectable; the arithmetic that has to agree is the same (`toc_len` inside the file, leaf lengths summing to the value area, `blocks = composites + 1`, table ≤ 65 535 bytes).

**4. It is layout v3, not v2 plus a tweak.** The stamp moves (ADR-0042) and a v2 segment is not read: the migration is the replay path ADR-0056 §5 already defines — every row that has a segment is decoded under the declaration that wrote it and re-encoded through the new build's write path.

**5. The codec leaves `okm-core`, and the value vocabulary goes with it.** The segment codec (`encode` / `decode`) is used by more than one crate — `okm-core` itself (`document.rs`, `model/graph.rs`) and `okm-dynamic` (`graph.rs`, `plan_fields.rs`) — and its currency, `DynamicValue`, is consumed further out still (`okm-dynamic` re-exports it as `Value`; `okm-lake` and the bindings build on it). The rule this ADR adopts is the plain one: **a codec one crate uses is a module in that crate; a codec two or more crates use is a crate of its own.** It is also ADR-0059's axis test — the unit of change should equal the unit that changes, and value types plus layout versions move on their own schedule, not the store's.

The crate cannot be the codec alone: its interface type is `DynamicValue`, so a codec crate that reached back into `okm-core` for its own argument type would be a cycle. The new crate is therefore the schema-free value vocabulary and its wire codec — `okm-value`: `DynamicValue`, `DynamicField`, `ValueMap`, `NameResolver`, `CodecError`, and `encode` / `decode`. `okm-core` depends on it; nothing is re-exported under the old paths (ADR-0043's posture: a rename leaves no alias behind).

**6. What this is not.** `okm-value` is a code home, not a specification and not an interchange format. The segment's bytes remain a function of `(value, vocabulary)` — field ids come from the owning table's vocabulary, allocated on first sight — so the format is not canonical across stores and is not offered as one. A standalone format would need names inline (losing the economy) or a shared mutable vocabulary (becoming a stateful protocol); that question is settled elsewhere and this crate does not reopen it.

## Rejected

**Width-carrying types (`u8`/`u16`/`u32`/`u64` as separate type codes).** It saves the same single byte for numbers whose magnitude exactly fills a power-of-two width — but it spends the bits the length needs: five bits of type leave two bits of length, so every body of three bytes or more (every string, every large integer) pays an escape, and the cost moves from numbers to strings. The one ability it adds over this decision — the declared width appearing on the wire — requires padding, so a `u64` holding 7 costs 8 body bytes against 3 today. It fails the Pareto test this ADR is built on.

**Implicit length for the three fixed-width types only, two bytes otherwise.** Saves one byte on `Bool`/`Null`/`F64` and nothing on numbers, for the same format generation. A third of the available gain at the same price.

**Pad every numeric to its declared width.** Measured loss on exactly the values that dominate metadata-shaped documents (a `u64` holding 7 becomes 8 body bytes). ADR-0057's widthless choice stands.

**Put the codec in `okm-wire`.** That crate owns the frame protocol — op tags, keys, values as opaque bytes — and has no value semantics at all. The value codec has no transport concern and is on the store path; the axes differ.

**Keep it in `okm-core` because it already sits in one file.** A file is not a boundary; the change cadence is. Keeping the codec there makes "one place to change" a habit, and the crate makes it a property.

**Offsets, fixed-width entries, a block directory.** Unchanged refusal (ADR-0057): there is no partial read — `VirtualStorage::get` returns the whole value — so an offset buys parse arithmetic that costs less than the bytes it adds.

## Consequences

- Every store holding a dynamic segment is re-encoded through the replay path (ADR-0056 §5). Tables with only declared fields are unaffected.
- `okm_core::model::obj_dynamic` and `okm_core::obj_segment` paths become `okm_value::…`: breaking for `okm-dynamic`, `okm-lake`, the bindings, and core's own call sites. `ValueMap` / `NameResolver` / `CodecError` travel with `DynamicValue`, so the trait implementations that take them move too.
- "A leaf entry is byte-identical to the old inline frame header" (ADR-0057 §6) stops being true; the internals page, its worked hex example and MODELING's grammar line follow.
- The decoder keeps one rule: bit 7 set means no length; otherwise len code 7 means a `put_len` varint follows; otherwise the code is the length. One escape branch is the whole new code path.
- An earlier one-byte header idea — a length-less fixed-width type set with widths in the type — is recorded here as rejected, so the next review does not re-derive it.

## Related records

ADR-0012 (the value vocabulary and its append-only type numbers), ADR-0042 (a layout change is a new stamp), ADR-0043 (no aliases under renamed paths), ADR-0056 §5/§6 (replay as the migration path), ADR-0057 (the table this header sits in), ADR-0059 (crate boundaries and the dependency-closure check). The bytes are documented in [the dynamic segment page](../internals/dynamic-segment.md); this record is the decision, that page is the mechanism.