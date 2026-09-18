# ADR-0016: 4-Byte Entry Head and Segment-Numbered Slots

Date: 2026-09-18
Status: Accepted
Supersedes: the slot allocation sections of ADR-0005 (1-byte slot), ADR-0011 (edge slots 14/15), and ADR-0012 (flat fixed-role slot table). See ADR-0015 for the junction residency decision this builds on.

## Context

Every derived entry (index, reduce, junction) hangs off a key headed by
`[ns 2B][slot 1B]`. The 1-byte slot grew by accretion: fixed roles own 0–15,
declared indexes/reduces continue from 16 upward, edges were pinned to 14/15
(ADR-0011), and nothing separates the entry *kinds* structurally. Segment
membership is a convention enforced by discipline, not by the key layout —
a slot byte says nothing about what kind of entry it addresses.

Capacity is also mismatched to its consumers: the u8 slot gives every role
the same 256-entry budget, while real consumption is wildly asymmetric
(fixed roles are a handful; declared indexes can grow with declarations;
junctions pair per relationship).

## Decision

The entry head widens to 4 bytes — **ns u16 + slot u16, both big-endian** —
and the slot's internals are split structurally:

```text
slot u16 = [segment 4 bit][counter 12 bit]

Segment (high nibble):
0x0  document-self  (counter: 0 primary, 1 dynamic, 2/3 dictionary,
                     4–4095 buffer)
0x1  declared index  (counter = declaration order)
0x2  reduce          (counter = declaration order, independent —
                     no longer continues the index counter)
0x3  junction        (counter = junction discriminator, declared)
0x4–0xB  reserved (derived / relation extensions)
0xC–0xF  reserved (system)
```

- **Segment extraction is structural**: `slot >> 12`. What was a numeric
  range convention is now a typed dispatch — misuse of one segment's slot
  for another's entry is a layout error, not a discipline failure.
- **Counter width grows**: each segment holds 4096 entries, wider than the
  old flat 256. Declared-index capacity per collection goes from 240 to
  4096; junctions get 4096 discriminators per endpoint pair.
- **16 segments are enough as an enumeration**: segments are entry *kinds*,
  not space allocations. Exhausting 16 kinds would be a semantic revolution
  warranting a version migration regardless.
- The full head is one u32 in implementations: `((ns as u32) << 16) | slot`.
  Scan-prefix construction and segment dispatch are both shifts, no
  lookup tables.
- Non-primary entries grow by one key byte. Against multi-byte identity
  payloads this is invisible; the primary slot (segment 0x0, counter 0)
  keeps the document's own key unchanged in shape.

## Junction placement (with ADR-0015)

A junction writes **one one-way entry in each endpoint's collection ns** —
two entries total, no third ns:

```text
[ns_a 2B][slot 0x3nnn?][A·identity][B·identity]   in A's collection
[ns_b 2B][slot 0x3nnn?][B·identity][A·identity]   in B's collection
```

`nnn` is the junction discriminator (`#[ok_junction(n)]`), separating
multiple junctions over the same endpoint pair. Both identities are in
the entry: the LOCAL endpoint's identity leads so the scan prefix
`[ns][slot][local identity]` can match, the PEER's identity is the
suffix. The discriminator's lowest bit carries the direction
(`slot = 0x3000 | (n << 1) | dir`) — structurally necessary only for
self-reflexive junctions (both endpoints in one ns), where without it
the two directions' scan prefixes would be identical and every scan
would cross-match; endpoint-distinct junctions never observe it, each
ns hosting exactly one direction. This supersedes ADR-0011's
forward/reverse slot pair (14/15) without resurrecting it: direction is
one bit inside the junction counter, not a slot pair.

The junction declaration references **document types, not key types**:

```rust
#[derive(JunctionEncode)]
#[ok_junction(1)]
struct Membership {
    user: User,
    org: Org,
}
```

The derive resolves `<User as Document>::Key` for identity encoding and
`<User as Document>::NS_PREFIX` for the endpoint ns. ns is declared once,
on the document; the junction re-states nothing.

## Consequences

- **Breaking key-format change**: every non-primary entry key grows by one
  byte; all hex-locked tests update. Zero-cost window: pre-release, no
  external data.
- Reduce no longer continues the index counter — the two declarations are
  independent quotas.
- `SlotMap` (schema export) becomes u16 fields with a single `junction`
  slot; `edge_fwd`/`edge_rev` disappear.
- The `KvIndex::SLOT` / `KvJunction` slot types widen to u16.
