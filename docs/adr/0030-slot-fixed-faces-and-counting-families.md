# ADR-0030: Slot — one byte per fixed face, two for counting families

**Status**: Accepted (2026-10-17) — design; implementation pending, see
Consequences
**Date**: 2026-10-17
**Chinese**: [0030-slot-fixed-faces-and-counting-families.zh-CN.md](0030-slot-fixed-faces-and-counting-families.zh-CN.md)
**Supersedes**: the segment table of ADR-0016 (the 4-byte head survives for
counting families; the slot's width now depends on the family)
**Depends on**: ADR-0005 (index slots), ADR-0012 (object model and field
vocabulary), ADR-0016 (head and segments), ADR-0017 (graph faces)

## Context

Every entry key today pays a 2-byte slot — `[ns 2B][slot 2B]`, a 4-bit
segment plus a 12-bit counter (ADR-0016). The counter was bought as
capacity headroom: 4096 per segment, declared-index capacity 240 → 4096.

Measured against its consumers, of the twelve slots in use **nine are a
single face**: the primary, the obj dynamic segment, both directions of
the field-name vocabulary, and the graph's five faces (update 2026-10-17:
the graph's predicate vocabulary added two more fixed faces — 0x04/0x05 —
so twelve are single faces today). Their counter is
constant — 0/1/2/3 for the document-self bucket, 1 for the graph's faces.
It carries no information and costs one byte on every such key.

Those nine also encode the same kind of thing two different ways: the four
document-self faces are counters inside segment 0x0 (a fossil of the
pre-0016 flat role table, where roles 0–15 were one numbering space), while
the five graph faces each got a whole segment (`0x4001`…`0x8001`, counter
pinned at 1). One face, one number would be the uniform rule.

The entries paying for it are the most numerous in the store: the primary
(one per document, one per edge) and the graph's traversal faces (0x07–0x0A,
thousands per edge collection). ADR-0017 §2 priced one byte on a reference
at ~5% and took it seriously; here the byte sits on every primary key and
every traversal entry.

## Decision

### 1. The slot is self-delimiting — the family decides its width

The slot's first byte says whether a counter follows:

```text
fixed face        first byte high nibble = 0    slot = 1 byte    [ns 2B][0x0F]
counting family   first byte high nibble != 0   slot = 2 bytes   [ns 2B][0x1n][nn]
                                                                 counter = (n << 8) | nn
```

In binary, the twelve slots in use, before → after:

```text
                         today                     slot bytes   after           hex
0x0000 primary           0000 0000 0000 0000       2 → 1        0000 0000       00
0x0001 obj dynamic seg   0000 0000 0000 0001       2 → 1        0000 0001       01
0x0002 dict id → name    0000 0000 0000 0010       2 → 1        0000 0010       02
0x0003 dict name → id    0000 0000 0000 0011       2 → 1        0000 0011       03
0x1001 index #1          0001 0000 0000 0001       2 → 2        0001 0000 0000 0001   10 01
0x1002 index #2          0001 0000 0000 0010       2 → 2        0001 0000 0000 0010   10 02
0x2001 reduce #1         0010 0000 0000 0001       2 → 2        0010 0000 0000 0001   20 01
0x3002 junction(n=1,dir=0) 0011 0000 0000 0010     2 → 2        0011 0000 0000 0010   30 02
0x4001 edge kind index   0100 0000 0000 0001       2 → 1        0000 0100       04
0x5001 edge out          0101 0000 0000 0001       2 → 1        0000 0101       05
0x6001 edge in           0110 0000 0000 0001       2 → 1        0000 0110       06
0x7001 edge kind+out     0111 0000 0000 0001       2 → 1        0000 0111       07
0x8001 edge kind+in      1000 0000 0000 0001       2 → 1        0000 1000       08
```

The right-hand nine rows read 1 byte: their high nibble is the delimiter.
(Update 2026-10-17: the graph's faces then moved again — 0x06–0x0A — and its
predicate vocabulary took 0x04/0x05, ADR-0017 §3. The nine MIGRATED faces
below are the mapping record; the allocation today is the table in §2.)
The three counting rows keep today's bytes exactly.

In the implementation the width follows from the slot's **value**: a slot
≤ 0x0F is written as one byte, anything else as two big-endian bytes (no
family number is 0, so nothing else can land in 0x00–0x0F). `write_slot`
asserts that rule, and `slot_len` gives the width — both live in
`okm-core::model::index` next to the constants, so every site that
assembles a key writes a slot the same way.

### 2. The fixed-face table (1 byte, 16 numbers)

```text
0x00  primary                      document collection or edge collection
0x01  obj dynamic segment          [ns][0x01][pkey]                            value = nTLV frames
0x02  field-name vocabulary        [ns][0x02][field-id]                        → name
0x03  field-name vocabulary        [ns][0x03][name]                            → field-id
0x04  predicate vocabulary         [ns][0x04][predicate_id]                    → name
0x05  predicate vocabulary         [ns][0x05][name]                            → predicate_id
0x06  graph predicate index        [ns][0x06][predicate_id][edge_id]
0x07  graph out                    [ns][0x07][src NodeRef][edge_id]
0x08  graph in                     [ns][0x08][dst NodeRef][edge_id]
0x09  graph predicate+out          [ns][0x09][predicate_id][src ref][edge_id]
0x0A  graph predicate+in           [ns][0x0A][predicate_id][dst ref][edge_id]
0x0B  vector index snapshot        [ns][0x0B][shard u16 BE]     → built graph (okm-vector cache)
0x0C–0x0F                      free for future fixed faces
(Update 2026-10-17: rows are grouped by ROLE — a vocabulary precedes the
faces that read its ids; the NUMBERS are the mechanism.)
```

**Update 2026-10-17: the PREDICATE VOCABULARY holds its own pair (0x04/0x05).**
A collection's token vocabulary beyond field names — a graph's predicates, a
node collection's kind tokens — is no longer the payload's field-name
vocabulary (0x02/0x03): listing it is one prefix scan of 0x04, and two
allocating caches can no longer hand one id to two names (ADR-0017 §3). The graph's declared attribute faces are
ordinary declared indexes, family 0x1, in the edge collection's own ns.

### 3. The counting families (2 bytes, 12-bit counter, numbers unchanged)

```text
0x1nnn  declared indexes   counter = declaration order (from 1 → 4095 per collection)
0x2nnn  reduces            counter = declaration order, independent quota (4095)
0x3nnn  junctions          counter = discriminator << 1 | direction
                           discriminator 11 bits → 2048 per endpoint pair
segments 0x4–0x0F          free (12 segments)
```

Not one of these numbers moves, and neither does the counter width: a
collection keeps 4095 declared indexes, 4095 reduces and 2048 junction
discriminators, exactly as today.

### 4. The delimiter is a fact about the byte, not a lookup

`b & 0xF0 == 0` reads one slot byte, anything else reads two. The two
spaces cannot overlap on the first byte (0x00–0x0F versus ≥0x10), so a
family-prefix scan is exact without consulting the table: `[ns][0x07]`
cannot match a counting family, `[ns][0x1n][nn]` cannot match a fixed
face. Being a property of the byte rather than of a maintained list is the
same posture as the ns escape byte (`0xFF`, ADR-0012) and the self-
describing endpoint references (ADR-0017 §2).

### 5. Vocabulary

"Segment" now means the counting families' number only (the high nibble of
their first byte). A single-face family is a **fixed face number**: one
byte, no counter, nothing to enumerate.

## Alternatives considered

- **A flat 1-byte slot, families carved out as level-1 ranges** (fixed
  faces first, then one range each for indexes / reduces / junctions).
  Rejected: a level-1 range split presupposes how each family will grow.
  The pre-0016 table's two-ended "heap/stack" band (fixed roles growing up
  from 0, edges pinned at 14/15 growing down, 4–13 as unassigned buffer)
  could only delimit two categories — a third has no boundary until the
  two ends meet. It also caps the families that genuinely count at
  whatever the split left them.
- **Promotion without the width change** (fixed faces get their own
  segments, everything keeps 2-byte slots). Rejected: it renumbers the
  counting families (indexes/reduces/junctions to 0x9/0xA/0xB, forcing a
  junction migration) and still pays the dead counter byte on the most
  numerous entries. Its only gain is vocabulary, bought with a harder
  migration.
- **Moving the delimiter to widen the fixed-face space** (e.g. first byte
  < 0x80 → 1 byte, giving 128 fixed faces and 8 counting families). Not
  taken today: 12 of 16 fixed faces and 3 of 15 counting families are in
  use. Recorded as the growth path — `k` is the only knob: top `k` bits
  zero → `2^(8−k)` fixed faces, `2^k − 1` counting families.
- **A 3-byte slot for counting families** (family byte + full u16 counter,
  e.g. 0x0100_01). Rejected: it makes index entries longer than today to
  buy counter headroom nothing consumes.

## Boundaries

- **A fixed face carries no counter.** If a family that is one face today
  ever needs instances, it becomes a counting family (take a segment from
  0x4–0x0F) — it does not reuse its fixed-face number for sub-kinds. New
  single-face kinds take the next free fixed-face number (5 left).
- **One counter width, 12 bits.** There is no per-family counter width;
  the family byte's low nibble belongs to the counter.
- **Cap wording corrected**: declared indexes and reduces are **4095 per
  collection** (the counter's 4096 values with 0 reserved), not 4096. The
  old text's 4096 would collide with the next family's segment.
- The head is no longer uniform across families (3 or 4 bytes). Family is
  a compile-time constant at every site, so prefix assembly and the
  primary-key tail slicing stay static arithmetic — no runtime probing.

## Consequences

- **Key-format break, pre-release.** Primary and the graph's faces lose one
  byte; the dynamic segment and both dictionaries are renumbered
  (0x0001→0x01, 0x0002/0x0003→0x02/0x03); the graph's faces drop their
  counter (0x4001–0x8001 → 0x06–0x0A). Index, reduce and junction entries
  keep their exact bytes — including the junction byte assertions in
  MODELING and the slot values the bindings pass (0x1001/0x1002/0x2001).
  No external consumer exists (PLAN), so a rebuild covers it: derived
  entries rebuild from documents, the dynamic segment and the dictionaries
  ride the same rebuild.
- `SlotMap` (schema export) keeps its `u16` fields — the values are now
  0x00–0x0A for the fixed faces and 0x1nnn / 0x2nnn / 0x3nnn for the
  counting families, with the width implied by the value. The bindings'
  slot arguments are unchanged.
- ADR-0016's segment table is superseded; its head-width decision survives
  for the counting families, and its junction placement stands untouched.
- docs/internals/key-layout and slot-mechanism carry the layout tables;
  the mechanism text there is the pair of this decision.