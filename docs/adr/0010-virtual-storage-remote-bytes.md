# ADR-0010: VirtualStorage — the storage trait as the boundary, remote bytes over existing channels

Date: 2026-09-13
Status: Accepted. **Update 2026-09-12, rename shipped**: the trait is now
literally named `VirtualStorage` (module `storage`; async twin
`VirtualStorageAsync` on the slatedb path) — no alias layer left to
conceptualize through. Backend struct names (`FjallStore`, `SlatedbStore`) are unchanged; the test `MockStore` was later replaced by the `TestStore` engine matrix ( slatedb-mem / fjall / redb).

## Context

Three independent needs converge on the same mechanism:

1. **Krystallizer hosting on Aura** — Krystallizer's storage must be able to
   live on the Aura node (remote deployment) without changing its storage
   semantics. Its engine choice so far: mock / fjall / slatedb.
2. **Multi-language Actors need KV semantics** — Python/Steel Actors
   (embedded, zero-IPC topology) want OKM's encoding discipline without
   hand-assembling keys. They need a schema-driven dynamic codec.
3. **Multi-tenant isolation** — multiple applications share an Aura node's
   physical engine; isolation must be structural, not naming-filtered.

The first instinct — a "remote KV protocol" with a declared endpoint and a
dedicated TCP service — fails on all three counts: it forks a second
transport where channels already exist (Probe→control plane is an outbound
WS connection; in-realm calls are realm events), and it manufactures a
protocol where the data is already encoded bytes.

## Decision

### 1. The trait IS the boundary — VirtualStorage

The storage trait (`VirtualStorage`; shipped 2026-09-12 under this name —
put/get/del/scan_suffix/batch/commit_batch) speaks only in encoded bytes.
It IS the storage boundary point. Four backend shapes behind one trait:

- `mock` — BTreeMap (tests)
- `fjall` — local engine (feature)
- `slatedb` — S3-backed engine (feature)
- **remote** — ops travel over an existing channel; bytes in, bytes out

Adding a backend = one more `impl VirtualStorage`. The trait gains no methods.

### 2. The remote backend sends what is already encoded

The write path in OKM is already double-write (primary + index entries in
one batch). The wire frame is exactly that batch:
`commit_batch`'s `MemBatch` op list, hand-framed with counted lengths —
`[op][batch bytes]`.
No semantic parsing anywhere:

- **Sender** (e.g. Krystallizer): keys/values are already encoded at the
  trait boundary; the frame only wraps op boundaries. It knows nothing about
  the receiver's prefix.
- **Receiver**: prepend its declared namespace prefix, execute as a plain
  byte-level KV engine (one real WAL commit per batch), return scan results
  as raw bytes. It does not parse keys, does not know TLV frames, does not
  know that OKM exists. Storing garbage is indistinguishable from storing
  data — by design.
- **Read path** (`get`/`scan_suffix`) needs a response — how a request is
  correlated with its answer is the consumer's business, not the wire's.
  The sender side implements the trait (`get` returns `Option<Vec<u8>>`);
  what transport, what correlation id, what callback or blocking wait it
  uses under that signature is its own choice. The wire carries request
  and response frames; everything above is outside this ADR.
- **No version header on the wire.** Layout versioning is the sender's
  in-process concern (compile-time hex tests + `#[ok_layout(version)]`
  decode rejection). Aura does not run the sender's OKM and holds no layout
  knowledge; the only contract between the ends is the op byte + byte
  streams. (This supersedes an earlier draft that put a layout version in
  the frame header — wrong because it semantically coupled ends that are
  deliberately unrelated.)
- **No generic serialization library on the wire.** The frame is counted
  fields, not a protocol: the ops are already encoded at the trait
  boundary, so a frame only needs to mark op boundaries — tag + lengths +
  raw bytes, hand-parsed (~30 lines with boundary checks). serde/postcard
  would buy "counting the lengths for us" at the cost of two dependencies
  and a trait mechanism for nothing: byte-for-byte the same volume (varint
  length prefixes are the same bytes), same order of parse cost (per-op a
  few instructions vs a WAL commit's microseconds — unmeasurable), and a
  compression/semantics layer the boundary must NOT have (see §2's refusal
  of version headers for the same shape of reason). Value compression, if
  any, belongs to the field-wrapper layer (VarInt/Quant — where value
  semantics are known), never to the frame; the frame sees compressed
  bytes as just shorter bytes.
- **Frame layout** (hand-parsed, no total-length header — the transport
  (`Vec<u8>` message / TCP stream framing) already delimits; restating it
  inside the frame is redundant):

  ```text
  write frame: [op_count varint]
               per op: [tag+type nibble u8][key len LK][value len LV][key][value]
      tag nibble: put / delete / get / scan (4 shapes, 2 bits, reserved 2)
      length encoding (LK and LV alike), first byte:
        00xxxxxx                              inline (≤63 bytes)
        01xxxxxx + 1 byte                     14-bit
        10xxxxxx + 2 bytes                    22-bit
        11xxxxxx + 4 bytes                    32-bit
      (low 6 bits of the first byte are the value's high bits; the four
      width buckets absorb the "tiny inline" case — a 4-bit length bucket
      would never be hit since real keys are ns+payload ≥ tens of bytes)
  ```

  Shift/mask decode costs single-cycle pipeline instructions — the real
  cost of bit packing is one selector branch per field and a wider
  bad-frame test surface (maintenance, not speed). Malformed lengths
  exceeding the remaining bytes are rejected, never panic.
- **Atomicity**: one frame = one batch = one receiver WAL commit. Cross-batch
  ordering = channel ordering. No epochs, no negotiation.

### 3. OKM instances are unrelated; the receiver bridge is the only crossing

Aura's own application data, Krystallizer's memory graph, and future
Python/Steel Actors (dynamic OKM) each run their own in-process OKM with
their own declared schema. These instances share nothing. The ONLY crossing
point is Aura's remote-storage receiver, which serves byte streams to remote
VirtualStorage backends and understands none of their content.

Dynamic OKM does NOT cross this bridge: same-process Actors hold the engine
directly; the dynamic codec is a schema-driven encoder/decoder generator
built from `describe()`/`json_schema()` exports, used in-process.
Capability ceiling, permanent: no reduce/subscribe on the dynamic side
(fold/unfold are Rust compile-time logic; a dynamic rebuild would break
exactly-once).

Engine choice is per-assembly-point and freely mixable: a local Table may
bind a local engine, while another Table in the same process binds the
remote backend ("forwarding" — same semantics, engine chosen remote).
There is no mixing conflict to guard: the receiver prefix is wrapping the
receiver applies, not part of the sender's key shape, so local `[ns 2B]`
keys and hosted `[prefix][ns 2B]` bytes never meet in one engine unless
the receiver itself chooses to host them there. No reserved prefix
values.

### 4. Receiver declaration: `NestStorage` derive

The receiver is declared, not hand-wired. An empty struct annotated with
`NestStorage` (with `#[ok_ns]`) declares only its namespace (which app/prefix it
serves). The derive generates **no data methods** (no put/get/scan — there
is no row type to encode) and exactly one execution method
(`apply`): take frame → prepend declared prefix → plain engine
execution → return the response frame (Some for get/scan, None for
put/delete — the return value itself answers whether a reply exists).

This pins three things at once: prefix source (declared, not scattered
config), executor shape (macro-generated, not hand-assembled), and
impossibility of prefix escape (the handle is bound to the prefix at
construction; crossing namespaces is not expressible — physical separation,
not naming filters). Same discipline as `#[ok_subscribe]`: the annotation
declares a fact; the macro emits the implementation.

### 5. Multi-tenancy: receiver-side prefix, one ns per instance

Multi-tenancy has exactly one database-level mechanism: the receiver's
declared prefix. A remote OKM instance is one application = one domain
model = its own ns dictionary; to the receiver it is just another ns
occupying one prefix. There is no `app_id` layer inside OKM, no reserved
prefix values, and no multi-level ns declaration — if an application
partitions by tenant internally, `tenant_id` is a plain key field in its
structs (business sharding, same modeling for every tenant); only when
whole applications are isolated at the platform level does the receiver
host one `NestStorage` (with `#[ok_ns]`) executor per application, each with its own
declared prefix and its own sender-side ns dictionary behind it.

**Bare shard form.** The prefix is optional (`NestStorage::bare`): a bare
host executes frames byte-identical — the sender's keyspace IS the
engine's keyspace. This serves sharding: N shards of one business domain
(one binary deployed per shard, one ns dictionary, one encoding) each get
a bare host; sharding and routing belong to the orchestrator (e.g. Aura's
partition-key routing), and OKM adds zero checking or machinery. The
prerequisite is the domain-model consistency of the shards pointing at
the host — guaranteed by deployment, not decidable at runtime (deciding
it would be OKM semantics the host must not have). Bare and hosted hosts
may coexist on one engine: hosted segments are 2-byte-disjoint among
themselves, and hosted apps may themselves shard (several bare-hosted
instances of one declared app, routed by user/partition key). The only
collision surface is byte coincidence — a bare instance's ns-table bytes
(`[0, N]...`) matching a hosted segment's prefix `[0, M]` when N = M —
which the orchestrator avoids by allocating hosted segment numbers off
the bare shards' in-domain ns numbers. One allocation at the layer that
owns the global view, not a runtime check in the host (a bare host does
not know hosted segments exist, and must not).

The physical key on the receiver's engine is a pure concatenation —
receiver bytes first, sender bytes after, order never adjusted:

```text
[receiver prefix][ ns 2B ][ sender's key payload ... ]
 └─ receiver ──┘  └────── sender bytes, untouched ─────┘
```

The receiver knows only its prefix; the ns segment is the sender's —
the receiver prepends its prefix and never parses what follows; ns bytes
pass through opaque. Knowledge asymmetry is the isolation mechanism: the
receiver cannot route past what it cannot see, and the sender cannot
escape the prefix it does not hold (the `NestStorage` (with `#[ok_ns]`) handle is bound
to the prefix at construction). ADR-0002's "if tenants exist" clause is
thereby **promoted from sketch to adopted mechanism**; its rejection of
per-tenant outer isolation was premised on "no user-programmable query
surface", which the dynamic-execution mode changes — this ADR is the
revision trigger. The ns dictionary discipline itself (compile-time,
append-only, u16) is untouched.

### 6. Transport is a backend-internal detail

Same process: direct engine handle (no remote backend at all).
Same machine, cross process: in-process channel / UDS.
Cross machine: the existing outbound WS connection (Probe case) or realm
events (in-realm case). The sender holds "an object implementing the trait";
physical topology is fixed at assembly time. No declared endpoint, no
dedicated listener, no second protocol.

Symmetrically, the receiver does not know arrival paths either. The
`NestStorage` (with `#[ok_ns]`) executor's surface is exactly one method — frame in, results
out; who called it and through which channel is the caller's business.
Read correlation (which response answers which request) lives on the
sender side of the executor, same as write. A TCP + hand-parsed client is
the reference example; it is an example, not part of the contract.
Transport diversity exists only on the two outsides of the executor and
never leaks into it.

## Consequences

- Krystallizer's storage config becomes four-way: mock / fjall / slatedb /
  virtual(→Aura). OKM semantic layers (Table/index/reduce/events) unchanged.
- Aura gains a Storage Actor hosting `NestStorage` (with `#[ok_ns]`) executors: one declared
  instance per application (one declared prefix each), frames arrive from
  VirtualStorage backends or realm events; same-machine callers connect
  in-process.
- The frame format is minimal and stable (`op + bytes`); it is NOT a
  compatibility surface between OKM instances — only between a sender's
  trait boundary and a receiver's engine. Changing OKM encoding requires no
  frame changes.
- Write-side seriality is the receiver engine's ordinary single-writer
  behavior, unchanged by remoteness.
