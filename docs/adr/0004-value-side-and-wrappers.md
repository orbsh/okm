# ADR-0004: Value side — versioned payloads, TLV extensions, and field wrappers

Date: 2026-09-06
Status: Superseded at the macro level by [ADR-0006](0006-row-node-model.md) (the standalone `ValueEncode` derive is cancelled; its encoding rules live on as the payload half of `RowEncode`). The mechanisms below remain authoritative.

## Context

The key layer is zero-waste (ns dictionary + fixed-width offsets), but the value side serialized with postcard is **non-self-describing** — fields are decoded by position, not tag. This makes "add a field" a structural cost of the whole-row packed layout: old data carries no field positions, so a new reader cannot skip an unrecognized tail. The fix is not a different layout but an evolvable serialization format. OKM reserves two levels of mechanism, plus a field-level wrapper system on the key side.

## L1: Versioned payload (simplest)

A 1-byte `schema_version` header (`u8` — width locked to the chosen type's byte width, same decision family as Enum/ns: version counts never approach 256; 0 reserved for the unversioned initial layout):

```rust
#[derive(ValueEncode)]
#[kv_version(2)]
pub struct AgentMemoryValue {
    pub content: [u8; 64],
    pub score:   i32,
}
```

The macro emits `encode_value` / `decode_value`; `decode_value` matches the version byte and dispatches to the layout decoder for that version. **Lazy migration**: `decode_v1` upgrades in memory on read; unread old records stay in the old format and never burn write bandwidth — impossible for SQL `ALTER TABLE`, which must rewrite every row. Discipline: fields may only be **appended at the tail**; mid-insert breaks old readers and is blocked by the version bump.

## L2: Tagged/TLV extension section (main axis)

L1's pain: every new field bumps the version and requires an old-version decoder. TLV demotes "evolution" from version dispatch to **field self-description**: the payload splits into a fixed-width hot section (existing `KeyEncode`-style direct offsets, position contract unchanged) + a tagged extension section (`field_id: u8 + len: u16 + bytes`):

```
[ ver:1B | content:64B | score:4B | ext_count:u8 | (id:1B len:2B bytes)... ]
  └── hot section, compile-time offsets ──┘  └── tagged extensions, skip-read ──┘
```

- **Old readers** read to the end of the hot section and skip the whole extension section by `ext_count` + per-item `len` — new fields require zero migration and no old-version decoders.
- **New readers** scan tagged items by `id`; unknown ids are skipped (forward compatible).
- **Extension ids are manually assigned** by the encoder, never auto-numbered — auto-numbering is again cross-item state collection, violating the "macros see one item" constraint; manual ids also ensure a deleted field's id is never reused and misreads cannot happen.
- **Cost**: 3-byte header per item + 1-byte `ext_count`; the variable-length extension breaks the fixed capacity budget (same strategy as String fields: `encode_value` falls back to a runtime cursor; the key layer is unaffected — extensions exist only in values).

## L3: Hot/cold section promotion (on demand)

Extension-section reads are skip-scans (O(field count)). When an extension field becomes a high-frequency single-field hot path, **promote it into the hot section**: move it to the tail of the hot field list. For old readers this is a layout change → bump the version, one explicit migration, caught by hex tests, never silent.

## Field-level wrappers (key side, orthogonal)

Key fields can be shrunk by wrapper types — the field type itself is the contract, declaration is automatic encode/decode:

| wrapper | dimension | encoding | read path |
|:--|:--|:--|:--|
| `Enum<T>` | cardinality | value → number (width locked to `T`'s byte width) | O(1) table lookup |
| `Offset<T>` | distribution | `value − base` (signed; base near present) | base + offset |
| `Delta<T>` | sequence | diff vs previous (pairs well with VarInt) | sequential accumulate |
| `VarInt<T>` | width | small values 1 byte, grows (LEB128) | continuation-bit walk |
| `Rle<T>` | repetition | runs → `(value, count)` | expand |
| `Quant<T>` | magnitude | time bucketing / precision drop (ns→ms) | rescale |
| `Reverse<T>` | sort direction | **bitwise complement** (`!bits.to_be_bytes()`) — byte-order scan becomes descending; prefix scan's first hit is the newest | invert again |

`Reverse<T>` is guarded at compile time by a `Reversible` trait (blanket impl over fixed-width integers only). Floats are excluded: IEEE 754 byte order is not order-isomorphic to numeric order (negative bit patterns compare larger), so bitwise inversion produces silently misordered bytes — a floating-point reverse must first `Quant` into an integer.

**Enum width locks to `T`'s byte width, never derived from cardinality**: macros see one item and cannot count enum variants globally; runtime derivation re-defines historical layouts when a variant fills the current width (same disease as the rejected ns width derivation). The ~2% overhead of fixed width buys "adding variants never changes layout"; when variants approach the width limit, the encoder explicitly upgrades `Enum<u8>` → `Enum<u16>` with a versioned lazy migration — a visible action caught by hex tests, not silent drift.

Wrappers are **compression semantics** (how a field is stored), orthogonal to **structure semantics** (key layout / field order); they compose (`Offset<i32>` fields still participate in prefix scans). Wrapper vs TLV: wrapper is key-layer compression; TLV is value-layer structure.

## Relationship to engine compression

Disk block compression (LZ4) belongs to the engine; variable-length wrappers solve the in-memory representation tension. The division of labor and the full value-granularity argument (whole-row vs field-per-key) live in the [KV Storage Engine essay](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md).
