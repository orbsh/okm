# ARCHITECTURE — the two-layer declaration machine

> 中文版：[ARCHITECTURE.zh-CN.md](ARCHITECTURE.zh-CN.md)。

OKM is a KV counterpart of ORM. This document explains its central
mechanism: how a Rust struct declaration becomes KV bytes, and why the
implementation is split the way it is. Normative modeling guidance
lives in [MODELING](MODELING.md); design decisions live in
[docs/adr/](adr/).

## The two layers: compile-time expansion + constant tables

Every feature in OKM crosses two layers, and the split is deliberate:

**Layer 1 — derive expansion (hot path).** The derive macros
(`KeyEncode`, `DocumentEncode`, `JunctionEncode`) expand a struct
declaration into *direct byte-manipulation code* —
`extend_from_slice(&self.field.to_be_bytes())` sequences with no
intermediate representation, no runtime schema walk, no reflection.
This is the fast path: put/get encode and decode at pure
pointer-arithmetic speed, the same code a hand-written codec would
produce.

**Layer 2 — constant tables (everything else).** Alongside the encode
code, the derive emits **associated constants on the type** —
`PAYLOAD_FIELDS` (name, width per field), `FIELDS` (typed field
descriptors), `HOT_WIDTH`, `NS_PREFIX`, `SLOT` per access method,
`DEFAULTS`, `FIELD_CONTRACTS`. These constants are the *reflection
surface*: everything that is NOT the hot path reads them instead of
re-deriving the declaration.

```text
#[derive(DocumentEncode)]
#[ok_ns(9)]
#[ok_index(by_a { fields(x) })]
struct User { x: u32, name: String }
        │ derive
        ▼
Layer 1 (code)                    Layer 2 (constants)
encode_payload()                  PAYLOAD_FIELDS: [("x",4),("name",0)]
decode_payload()                  FIELDS: [FieldDesc; 2]
index_entries()                   HOT_WIDTH: 4
__okm_encode_named()              NS_PREFIX: [0,9]
        │                         by_a::SLOT: 0x1001
        ▼                         DEFAULTS / FIELD_CONTRACTS
Collection::put/get/scan ───────── both layers meet here
        │
VirtualStorage (Fjall / redb / slatedb)
```

**Why the split matters**: consumers of the declaration — the schema
export (`TableSchema::of`), the dynamic reader (okm-dynamic), the
Arrow/Parquet bridge, snapshot tooling — need *knowledge* of the
schema, not its code. They read the constants. Adding a consumer never
requires touching the derive; adding a field requires only that the
derive knows how to emit one more constant entry. The two layers are
independently extensible, coupled only by the `Document` trait's
associated items.

This is also why the dynamic mode can exist at all: `TableSchema`
(assembled from the constants) is a complete, serializable description
of the schema — the Python and Steel bindings build collections from
it without any Rust type in sight.

## Module layout

```text
okm-core/src/
├── model/    declaration-facing core — key codecs, the Document trait
│             and Collection, indexes, junctions, reduces, schema
│             export, dynamic-segment codec, wrapper types (wrappers/)
├── engine/   the KV boundary — VirtualStorage abstraction and its
│             adapters (Fjall / redb / slatedb), TestStore, remote
│             nesting. cfg-gated per engine; the model layer sees only
│             the trait.
├── bridge/   exports to the outside world — Arrow/Parquet columnar
│             bridge, JSON-schema/snapshot tooling. Reads Layer-2
│             constants only.
└── subscribe write-path event emission (cross-cutting)
```

Dependency direction is one-way: `model` never names an engine; the
`bridge` layer never computes — it reads constants and formats.

## The key grammar

Every key shares one header discipline:

```text
[ (0xFF part 1B) ][ ns 2B BE ][ slot 2B BE ][ identity / fields ... ]
```

- **ns** (u16 BE): the collection's identity, declared once on the
  document (`#[ok_ns]`), full 16-bit space shared by all entry kinds.
- **slot** (u16 BE): 4-bit segment (entry kind — document-self / index /
  reduce / junction) + 12-bit in-segment counter. Segment membership is
  structural (`slot >> 12`), dispatched by shift, not convention
  (ADR-0016).
- **identity tail**: always the primary-key encoding (full or a
  declared truncation), so every derived entry is reversible to its
  source document.

Junctions are the one cross-collection entry kind: one one-way entry
per endpoint ns (two-ns residency, ADR-0015), direction carried by
which ns hosts the entry, a discriminator in the slot's low bits.

## The wire discipline

All variable-length framing uses one codec — the prefix-monotonic
varint (`wrappers/wire.rs`, P1/P2): byte comparison equals numeric
comparison, so `VarInt<T>` is index-segment legal, and every TLV
length prefix (dynamic-segment frames, declared cold frames, Vector
element LVs) costs 1–2 bytes instead of 4. Fixed-width things stay
fixed-width: hot-segment fields and index keys never use variable
encoding, because static offsets and sort order are the contracts that
make the fast path fast.

## What the derive does NOT do

- No I/O, ever. Every generated function is pure bytes-in/bytes-out.
- No engine knowledge. `Collection<S, K, R>` binds an engine at the
  assembly site; the derive cannot name one.
- No runtime registry. "The declaration IS the registry" — index
  slots, contracts, and defaults are constants read at compile time or
  assembled into `TableSchema` once.

## Where things go next

The PLAN tracks open work (Set type via inverted index, KDL schema
serialization as a low-priority alternative to JSON). The ADR series
records why each decision above is the way it is — ADR-0002 (ns
dictionary), ADR-0005 (index slots), ADR-0012 (object model +
dictionary), ADR-0015/0016 (relations + the 4-byte head).
