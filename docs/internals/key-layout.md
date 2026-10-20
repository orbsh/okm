# Key Layout: ns prefix and slot dispatch

Mechanism/implementation document: the first-level layout of every key that
lands in an engine — how the 2-byte ns prefix is encoded, how documents and
junctions share this level, and how the slot follows. Slot allocation
discipline and the append-only contract live in [slot
mechanism](slot-mechanism.md); decision records live in
[ADR-0002](../adr/0002-namespace-dictionary.md),
[ADR-0005](../adr/0005-secondary-index-slots.md),
[ADR-0016](../adr/0016-four-byte-head-and-slot-segments.md) (the entry head,
superseded for the fixed faces) and
[ADR-0030](../adr/0030-slot-fixed-faces-and-counting-families.md) — the
current allocation: a fixed face is one byte, a counting family keeps the
2-byte slot.

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
the engine, it never restates the ns (the ns vocabulary is code, ADR-0002;
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

## Level 2: the slot — one byte for a fixed face, two for a counting family

After the ns header comes the slot. Its first byte says whether a counter
follows:

```text
fixed face        first byte high nibble = 0    slot = 1 byte    [ns][0x0F]
counting family   first byte high nibble != 0   slot = 2 bytes   [ns][0x1n][nn]
                                                                 counter = (n << 8) | nn
```

**Fixed faces** (1 byte, 16 numbers, 12 in use) — a family that is a single
face, with nothing to enumerate. Big-endian is why the high nibble leads,
and why the rule holds: 0x00–0x0F can never equal a counting family's first
byte (≥ 0x10).

```text
0x00  primary                      [ns][0x00][pkey]                            value = TLV payload
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
0x0B  vector index snapshot        [ns][0x0B][shard u16 BE]    → built graph (okm-vector cache, droppable)
0x0C–0x0F  free for future fixed faces
(rows are grouped by ROLE: the two vocabularies first, then the graph faces
that read their ids — a vocabulary leads the faces it serves; the NUMBERS
are the mechanism.)
```

**Counting families** (2 bytes, 12-bit counter, numbers unchanged) — a family
whose instances are enumerated by declaration order or by a discriminator:

```text
0x1nnn  vocabulary: label → id  [ns][0x1nnn][label bytes]       nnn = the vocabulary's number
0x2nnn  vocabulary: id → label  [ns][0x2nnn][id bytes BE]       same nnn, other family
0x3nnn  vocabulary: cursor      [ns][0x3nnn] → next id BE       same nnn; one entry
0x4nnn  declared index          [ns][0x4nnn][index fields][pkey prefix]  nnn = declaration order
0x5nnn  reduce                  [ns][0x5nnn][group seg]         nnn = declaration order,
                                                                independent counter
0x6nnn  junction                [ns][0x6nnn][local id][peer id] nnn = discriminator << 1
                                                                | direction
segments 0x7–0x0F  free
```

A declared `Vocab<N>` field (ADR-0032) spends ONE number and occupies three
faces: 0x1nnn / 0x2nnn for the two directions and 0x3nnn for the allocation
cursor, so the three sit next to each other and the pairs stay recognisable. The direction is the family, never a parity
convention inside one family — `0x1001` and `0x2001` are the label → id and
id → label faces of the first categorical field, `0x1002`/`0x2002` the second.
The two collection-wide name domains are older than this scheme and keep their
fixed faces (0x02/0x03 field names, 0x04/0x05 predicate tokens).

The width rule is a fact about the byte (`b & 0xF0 == 0` reads one slot byte,
anything else reads two), not a lookup in a maintained table — the same
posture as the ns escape byte below and the self-describing endpoint
references (ADR-0017 §2). A family-prefix scan is therefore exact without
consulting anything: `[ns][0x07]` cannot match a counting family, and
`[ns][0x1n][nn]` cannot match a fixed face.

Counting capacity is 4096 per family: declared indexes and reduces occupy
1–4095 (0 reserved), junction uses the whole range for
`discriminator << 1 | direction` (2048 discriminators per endpoint pair). A
junction writes one one-way entry in each endpoint's ns (two-ns residency,
ADR-0015); direction rides in the counter's low bit — structurally needed
only for self-reflexive junctions, where both directions would otherwise
share one scan prefix.

The slot keeps "adding derived data inside a collection" from ever
encroaching on a neighboring collection's ns segment (ADR-0005), and its
first byte is structural in both forms: it delimits the slot and names the
family, so a fixed face's number can never be misread as a family's. The
superseded segment table (4-bit segment + 12-bit counter for every entry) is
in [ADR-0016](../adr/0016-four-byte-head-and-slot-segments.md). Allocation
discipline, the append-only contract, and the variable-width-field
constraints live in [slot mechanism](slot-mechanism.md).

**Partition prefix (optional, before the ns header)**: a collection declared
with `#[ok_partition(N)]` prepends one `[0xFF][N 1B]` segment to every key:

```text
partitioned entry  [ 0xFF ][ part 1B ][ ns 2B ][ slot … ][ ... ]
```

`0xFF` is the escape byte — legal ns headers (big-endian u16, first byte
constrained by the ns vocabulary to 0x00–0xFE) never start with it, so
partitioned and unpartitioned key spaces are **structurally disjoint** with
zero numbering coordination (partition(0) is rejected — omit the attribute
to have no segment). Semantics: partition is workload isolation (compaction
grouping), not an ownership boundary; ns remains the outermost ownership
level.

## Panorama

```text
document  [ (0xFF part 1B) ][ ns 2B ][ slot 1B (fixed face)     ][ ... ]
document  [ (0xFF part 1B) ][ ns 2B ][ slot 2B (counting family)][ ... ]
junction  [ ns_a 2B         ][ 0x3nnn ][ local id ][ peer id ]   in A's collection
junction  [ ns_b 2B         ][ 0x3nnn ][ local id ][ peer id ]   in B's collection
            ↑ one number space, one header discipline; the first slot byte
              delimits the slot and names the family — no transforms
```

## Where reduce lives

A reduce entry has no ns of its own — it parasitizes its host document's ns
segment, segment 0x2, with a counter independent of the index counter
(ADR-0016 removed the old chained coupling):

```text
reduce entry  [ ns 2B ][ 0x2nnn ][ group seg ]   value = acc encoding
```

For scans, the prefixes inside one ns segment enumerate everything derived
for this document: fixed faces 0x00–0x03 (primary, dynamic segment, both
field-name vocabulary directions), then counting families 0x1 indexes, 0x2
reduces, 0x3 junctions. A graph (edge collection) adds its own fixed faces:
the predicate vocabulary 0x04/0x05 (its predicates) and 0x06–0x0A (predicate index,
out/in edges, predicate±endpoint). A document collection that carries kind
tokens declares the same vocabulary pair — one rule, whichever collection
needs it. Mechanism in [reduce mechanism](reduce-mechanism.md).