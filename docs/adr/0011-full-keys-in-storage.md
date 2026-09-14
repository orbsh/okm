# ADR-0011: Full keys in storage — no prefix stripping

Date: 2026-09-14
Status: Accepted

## Context

Every byte key that reaches the engine is a **self-describing address**:

```text
[ns 2B][slot 1B][key payload]
```

`ns` is the table's declared namespace number (`#[kv_ns]`, ADR-0002), the
slot byte marks primary (0) vs index entries (ADR-0005), and the payload is
the encoded key or indexed fields. When a remote sender talks through a
declared `NestStorage`, the receiver prepends one more layer — its host
prefix — so bytes on the wire become
`[host prefix][ns 2B][slot 1B][payload]` on the engine (ADR-0010).

An alternative was raised: store **stripped keys** (`[key payload]` only)
and re-attach the prefix at query time. Storage would shrink by 3 bytes per
key. This ADR records why that is rejected.

## Decision

Keys are stored **with every prefix layer included**, exactly as the access
methods build them. No read or write path strips or re-attaches anything:
`primary_key` / `index_entries` compose the full key once (in the
derive-generated encode layer), and every consumer — get, scan, reduce
unfold, index back-reference, prune, edge traversal, remote frame execution
— uses that same byte sequence untouched.

## Rejected: strip-on-store, join-on-query

**What it saves.** 3 bytes per key. Even at tens of millions of rows this
is tens of megabytes — the cheapest bytes in the system, further multiplied
away by nothing (they sit in the same WAL/compaction copies regardless;
stripping saves a fixed 3-byte slice, not a copy).

**What it costs.**

1. **Isolation degrades from physical to logical.** Today two namespaces'
   keys are byte-disjoint: the engine's lexicographic order partitions them
   by construction, and a bare shard host can execute frames byte-identical
   precisely because the sender's bytes are already self-isolating. With
   stripped keys, isolation becomes "every query path remembers to join the
   right prefix" — a naming filter, the exact shape ADR-0002 and ADR-0010
   reject. Prefixes in the key make wrong states inexpressible; prefixes in
   the query path make them one forgotten join away from silent
   cross-namespace reads.

2. **Scans break.** Range scans rely on prefix contiguity in stored byte
   order: scanning `[ns][slot]` returns that index's whole entry set because
   those bytes lead every stored key. Strip them and the scan surface must
   re-derive per-call what contiguous range covers "this index", and a
   `NestStorage` receiver can no longer "prepend the declared prefix and
   never parse what follows" — it would need to understand key structure to
   place the host prefix correctly, destroying the knowledge asymmetry that
   ADR-0010 §5 establishes as the isolation mechanism.

3. **Engine sharing dies.** Several tables on one engine (test matrix,
   multi-tenant hosted forms) coexist because their full keys are
   byte-disjoint. Stripped keys collapse distinct tables' identical
   payloads onto one physical key.

4. **The join point multiplies.** Today the composition lives in exactly
   one place (derive-generated key construction). Stripping requires every
   read path to know which table/ns a key belongs to in order to re-join —
   turning one declaration into N scattered conventions.

**And the bytes are not redundant.** The slot byte unifies primary and
index headers (ADR-0005); the ns bytes are the physical carrier of the ns
dictionary. A key that cannot say where it belongs is not cheaper — it is
unaddressed.

## Consequences

- Key composition stays single-point (derive layer); no read path carries
  prefix-joining logic.
- Bare shard and hosted `NestStorage` forms both keep byte-identical
  execution semantics.
- Storage cost per key includes 3 fixed header bytes; accepted as the
  price of physical isolation. If key size ever matters at the margin,
  the lever is compressing the payload (ADR-0004 wrappers), never
  stripping the address.
