# Key Layout: ns prefix and level dispatch

Mechanism/implementation document: the first-level layout of every key that
lands in an engine — how the 2-byte ns prefix is encoded, how documents and
junctions share this level, and how the slot follows. Slot allocation
discipline and the append-only contract live in [slot
mechanism](slot-mechanism.md); decision records live in
[ADR-0002](../adr/0002-namespace-dictionary.md),
[ADR-0005](../adr/0005-secondary-index-slots.md), and
[ADR-0016](../adr/0016-four-byte-head-and-slot-segments.md) (the 4-byte head
and segment-numbered slots — the current allocation).

## Declaration point: ns hangs off the document, not the key

`#[ok_ns(N)]` is declared on the **document struct**, not on the key struct:

- the document is the collection's declaration point — its `#[ok_ref]` pins
  the key type, so `Collection<S, K, R>` is fully determined by the document;
  ns is part of that collection's identity;
- a key type carries no ns, which means **one key shape may legitimately
  serve several documents / collections**, each with its own ns number. With
  ns on the key this scenario is blocked at compile time and can only be
  worked around by declaring a second identical key type.

The derive (`okm-derive`) compiles `#[ok_ns(N)]` into
`Document::NS_PREFIX: &'static [u8]` (big-endian `[hi, lo]`; empty slice when
undeclared — a codec-only document that never materializes a collection).
`Collection::new(store)` takes no ns argument — the assembly point chooses
the engine, it never restates the ns (the ns dictionary is code, ADR-0002;
engine choice is per-assembly-point freedom, ADR-0010).

A junction declares no ns: its fields reference **document types**
(`user: User`), and the derive resolves `<User as Document>::NS_PREFIX` for
the endpoint ns (ADR-0015, ADR-0016). ns is declared once, on the document.

## Level 1: the 2-byte ns prefix, shared by all entries

Every entry key starts with the same uniform 2-byte big-endian header — the
ns raw value, written big-endian. Documents, indexes, reduces, and junctions
share the full 16-bit number space: no transform, no reserved half:

```text
[ ns 2B BE ] ...
```

## Level 2: the 2-byte slot (4-bit segment + 12-bit counter)

After the ns header comes a 2-byte big-endian slot: the high 4 bits are the
**segment number** (a structural dispatch of entry kinds, `slot >> 12`), the
low 12 bits are the in-segment counter. The full segment table and the
rationale live in [ADR-0016](../adr/0016-four-byte-head-and-slot-segments.md);
the current allocation:

```text
Segment 0x0  document-self   slot 0x0000 primary      [ns][0x0000][pkey]       value = TLV payload
                             slot 0x0001 obj dynamic  [ns][0x0001][pkey]       value = nTLV frames
                             slot 0x0002 field dict   [ns][0x0002][field-id]  → name
                             slot 0x0003 field dict   [ns][0x0003][name]      → field-id
                             slot 0x0004+  buffer (4092 entries)
Segment 0x1  declared index  [ns][0x1nnn][index fields][pkey prefix]  nnn = declaration order
Segment 0x2  reduce          [ns][0x2nnn][group seg]                  nnn = declaration order,
                                                                      independent counter
Segment 0x3  junction        [ns][0x3nnn][peer identity]              nnn = junction
                                                                      discriminator
Segments 0x4–0xB  reserved (derived / relation extensions)
Segments 0xC–0xF  reserved (system)
```

The segment number is an **enumeration** (entry kinds), not a space
allocation: 16 kinds match the scale of "document-self / a few derived /
relation / system"; the counter holds 4096 per segment, wider than the old
flat 256. A junction writes one one-way entry in each endpoint's ns
(two-ns residency, ADR-0015); direction is carried by which ns the entry
lives in, not by a slot bit — no forward/reverse slot pair anymore.

The slot segment keeps "adding derived data inside a collection" from ever
encroaching on a neighboring collection's ns segment (ADR-0005); segment
membership is a structural fact (a shift yields the kind), not a numbering
discipline. Allocation discipline, the append-only contract, and the
variable-width-field constraints live in [slot
mechanism](slot-mechanism.md).

**Partition prefix (optional, before the ns header)**: a collection declared
with `#[ok_partition(N)]` prepends one `[0xFF][N 1B]` segment to every key:

```text
partitioned entry  [ 0xFF ][ part 1B ][ ns 2B ][ slot 2B ][ ... ]
```

`0xFF` is the escape byte — legal ns headers (big-endian u16, first byte
constrained by the ns dictionary to 0x00–0xFE) never start with it, so
partitioned and unpartitioned key spaces are **structurally disjoint** with
zero numbering coordination (partition(0) is rejected — omit the attribute
to have no segment). Semantics: partition is workload isolation (compaction
grouping), not an ownership boundary; ns remains the outermost ownership
level.

## Panorama

```text
document  [ (0xFF part 1B) ][ ns 2B ][ slot 2B ][ ... ]   partition optional
junction  [ ns_a 2B         ][ 0x3nnn ][ peer identity ]        in A's collection
junction  [ ns_b 2B         ][ 0x3nnn ][ peer identity ]        in B's collection
            ↑ one number space, one header discipline; the slot segment
              dispatches entry kinds — no transforms
```

## Where reduce lives

A reduce entry has no ns of its own — it parasitizes its host document's ns
segment, segment 0x2, with a counter independent of the index counter
(ADR-0016 removed the old chained coupling):

```text
reduce entry  [ ns 2B ][ 0x2nnn ][ group seg ]   value = acc encoding
```

For scans, the prefix `[ns][slot]` inside one ns segment enumerates
everything derived for this document: segment 0 primary/dynamic/dictionary,
segment 1 indexes, segment 2 reduces, segment 3 junctions. Mechanism in
[reduce mechanism](reduce-mechanism.md).
