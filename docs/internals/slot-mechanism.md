# Slot Mechanism: in-namespace dispatch of derived entries

Mechanism/implementation document: how the slot dispatches derived entries
inside a 2-byte namespace, what the constraints are, and why. Modeling
discipline lives in [MODELING](../MODELING.md); decision records live in
[ADR-0005](../adr/0005-secondary-index-slots.md),
[ADR-0016](../adr/0016-four-byte-head-and-slot-segments.md) (the widened
head, superseded for the fixed faces) and
[ADR-0030](../adr/0030-slot-fixed-faces-and-counting-families.md) (the
current allocation: a fixed face is one byte — no counter — and only the
counting families carry the 2-byte slot).

## Entry layout

A collection's entries come in two shapes, decided by the slot's first byte:

```text
fixed face      [ ns 2B BE ][ slot 0x00–0x0F ]                             value = TLV payload (primary)
counting family [ ns 2B BE ][ slot 0x1nnn ][ index fields ][ pkey ]        value = includes TLV (declared index)
```

The discriminator = the 2-byte namespace + the slot: the ns segment scopes
the whole collection, and the slot's first byte either delimits the slot
(high nibble 0 — a fixed face, one number per face) or carries a family
number in its high nibble with a 12-bit counter in the rest. The full key
layout lives in [key layout](key-layout.md); this page expands only the slot
level. `#[ok_ns]` keeps its cleanest semantics — one number per collection,
the collection's ns allocation fully decoupled from the number of derived
entries (ADR-0005).

## Slot allocation

`SLOT` is a compile-time constant. A fixed face's slot is its one byte
(primary = `0x00`, `PRIMARY_SLOT`; the dictionary directions `0x02`/`0x03`;
the graph's faces `0x06`–`0x0A`). A counting family's slot is its family
number plus a counter allocated by **declaration order** — the declared-index
family is `0x1`, so the first `#[ok_index]` is `0x1001`, the second `0x1002`, …

```text
#[ok_ns(9)]
struct User { ... }
#[ok_index(by_a ...)]   →  SLOT=0x1001, entry head = [0,9,0x10,0x01]
#[ok_index(by_b ...)]   →  SLOT=0x1002, entry head = [0,9,0x10,0x02]
```

Reduces live in family 0x2, junctions in family 0x3 — each with its own
independent counter (ADR-0016 removed the old coupling where reduce
continued the index counter). Cross-collection isolation is guaranteed by
the manual ns dictionary (ADR-0002); the slot only isolates **within** a
collection. The per-collection cap is 4095 indexes, 4095 reduces and 2048
junction discriminators (a 12-bit counter with 0 reserved) — overflow is a
compile-time fact (literal overflow), never a runtime risk.

## What compiles to constants

The generic parameter `I: KvIndex` gives the scan path three pieces of
static information at compile time, zero runtime lookup:

- `SLOT`: for a counting family, bytes 3–4 of the scan prefix (the
  `[ns 2B][slot 2B]` head); a fixed face occupies byte 3 only;
- the index-field segment width: `encode_named` encodes payload fields
  big-endian in declaration order, widths determined by
  `KeyEncode`/field-type arithmetic;
- the key-prefix width: `key_prefix_width()` — `KEY_PREFIX` empty means
  `KEY_LEN` (full primary key); truncated means the summed width of the
  named subset.

The runtime does three things: assemble the prefix (`entry_prefix`), range
scan the engine (`scan_suffix`), and slice the primary-key encoding off the
entry key's tail by the known width and decode it.

## The append-only discipline

SLOT depending on declaration order means **the index declaration sequence
is a persisted contract**:

- Append-only. Inserting mid-sequence shifts every later index's slot;
  existing entries stay at their old slot positions and `scan::<I>` returns
  empty after the prefix moved — a silent error.
- **No deleting declarations directly** — deleting a middle declaration
  shifts all later slots down and new writes land on the previous
  declaration's existing entries (silent corruption, same class as the
  rejected additive derivation). The correct retirement path is the
  `deprecated` marker: `#[ok_index(by_old { … }, deprecated)]` keeps the
  slot occupied and generates no write/scan surface; historical entries are
  cleared explicitly by `Collection::prune_deprecated_slots()` (prefix
  scan-delete over `[ns][deprecated slot]`, idempotent, returns the count).
- No index backfill: an appended index only affects documents written after
  it; existing documents get no entries — full coverage requires a
  migration double-write.

Reordering equals changing the layout: rebuild from scratch or migration
double-write; there is no in-place reorder path.

## History

The original design (ADR-0005 body) weighed three shapes: 4-bit concatenation
(`ns << 4 | slot`, halving the ns space), the 1-byte slot byte (chosen then),
and the briefly-implemented **additive derivation**
(`index_ns = table_ns + SLOT`, saving the slot byte). The additive form's
fatal flaw: a table's ns allocation was no longer self-contained — assigning
ns=256 required reserving headroom for "future indexes", adding an index
could crash into the next table's segment, and the error was silent (two
tables' segments overlapping, scans still returning — the other table's
entries). 2026-09-10 returned to the 1-byte slot byte form.

2026-09-18: the slot widened to 2 bytes with segment numbering (ADR-0016) —
entry kinds promoted from a numbering convention to structural dispatch
(4-bit segment), counters widened to 4096 per segment; junctions landed in
segment 0x3 (two-ns residency, ADR-0015), superseding ADR-0011's 14/15
forward/reverse slot pair.

2026-10-17: the widening was paid back where nothing needed it (ADR-0030).
Nine of the twelve slots in use were a single face at landing (eleven today,
update 2026-10-17: the graph gained a predicate dictionary) — primary, dynamic
segment, both dictionary directions, the graph's five — and their counter
was constant. Fixed faces became one byte with their own numbers (0x00–0x0A)
and the segment table gave way to a two-shape layout: fixed face (1 byte) or
counting family (2 bytes, 12-bit counter, numbers unchanged — indexes and
reduces still start at 0x1001/0x2001, junctions still 0x3nnn). The
per-collection caps are 4095 indexes / 4095 reduces / 2048 junction
discriminators, and the earlier 4096 wording was one too many (the counter's
0 is reserved).