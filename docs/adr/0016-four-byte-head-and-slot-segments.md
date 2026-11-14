# ADR-0016: 4-Byte Entry Head and Segment-Numbered Slots

Date: 2026-09-18
Status: Accepted (2026-09-18). The entry head is `[ns 2B][slot]`, and the slot's width follows the family (ADR-0030): a fixed face is one byte with no counter, the counting families keep the 2-byte slot — a 4-bit segment number plus a 12-bit counter. The head-width decision below holds for the counting families; ADR-0030 owns the fixed-face table and the numbering.
Supersedes: the slot allocation sections of ADR-0005 (1-byte slot), ADR-0011 (edge slots 14/15), and ADR-0012 (flat fixed-role slot table).

## Context

Every derived entry (index, reduce) hangs off a key headed by
`[ns 2B][slot]`. The 1-byte slot had grown by accretion: fixed roles owned 0–15,
declared indexes/reduces continued from 16 upward, edges were pinned to 14/15
(ADR-0011), and nothing separated the entry *kinds* structurally. Segment
membership was a convention enforced by discipline, not by the key layout —
a slot byte said nothing about what kind of entry it addressed.

Capacity was also mismatched to its consumers: the u8 slot gave every role
the same 256-entry budget, while real consumption is wildly asymmetric
(fixed roles are a handful; declared indexes grow with declarations).

## Decision

For a counting family the entry head is 4 bytes — **ns u16 + slot u16, both
big-endian** — and the slot's internals are split structurally:

```text
slot u16 = [segment 4 bit][counter 12 bit]      (counting families)

0x2nnn  vocabulary, label → id  counter = the vocabulary's number
0x3nnn  vocabulary, id → label  same counter, other family
0x4nnn  declared index          counter = declaration order
0x5nnn  reduce                  counter = declaration order, independent quota
0x6nnn  lead face               counter = declaration order
```

(These are the numbers of the current band; ADR-0030 kept the two-byte counting
family and ADR-0033 widened the fixed-face band. Segment dispatch — `slot >> 12`
— is exactly the decision this ADR records, and it is unchanged.)

ADR-0030 owns the full assignment and the fixed-face table. A fixed face is not
a slot with a counter: it is one byte, `0x00–0x1F`, its own number and no
segment — primary `0x00`, the dynamic segment `0x01`, both vocabularies
`0x02–0x05`, the graph's faces `0x06–0x0D`, the handle registry `0x0E`/`0x0F`,
the Extension `0x1E`, the table meta `0x1F`.

- **Segment extraction is structural**: `slot >> 12` on a counting family. A
  numeric range convention becomes a typed dispatch — misuse of one segment's
  slot for another's entry is a layout error, not a discipline failure.
- **Counter width**: each counting family's counter holds 4096 entries. A
  collection carries 4095 declared indexes and 4095 reduces, each an
  independent quota.
- **16 segments are enough as an enumeration**: segments are entry *kinds*,
  not space allocations. Exhausting 16 kinds would be a semantic revolution
  warranting a version migration regardless.
- The full head is one u32 in implementations: `((ns as u32) << 16) | slot`.
  Scan-prefix construction and segment dispatch are both shifts, no
  lookup tables.
- A counting-family entry pays one more key byte than a fixed face. Against
  multi-byte identity payloads this is invisible; the primary (fixed face
  0x00, one byte) keeps the document's own key unchanged in shape.

## Consequences

- Counting-family entries carry a 2-byte slot, fixed faces one byte;
  hex-locked tests pin both widths.
- Reduce and index counters are independent quotas.
- `SlotMap` (schema export) carries u16 numbers: the fixed faces plus
  `declared_index_base` and `declared_reduce_base`.
- `KvIndex::SLOT` is a `u16` (`type Slot = u16`).
