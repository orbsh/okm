# ADR-0010: VirtualStorage — KvEngine as the storage boundary, remote bytes over existing channels

Date: 2026-09-13
Status: Accepted

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

`KvEngine` (put/get/del/scan_suffix/batch/commit_batch) already speaks only
in encoded bytes. It is hereby the storage boundary point, aliased/conceptualized
as **VirtualStorage**. Four backend shapes behind one trait:

- `mock` — BTreeMap (tests)
- `fjall` — local engine (feature)
- `slatedb` — S3-backed engine (feature)
- **remote** — ops travel over an existing channel; bytes in, bytes out

Adding a backend = one more `impl KvEngine`. The trait gains no methods.

### 2. The remote backend sends what is already encoded

The write path in OKM is already double-write (primary + index entries in
one batch). The wire frame is exactly that batch:
`commit_batch`'s `MemBatch` op list serialized (postcard) — `[op][batch bytes]`.
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
  in-process concern (compile-time hex tests + `#[kv_layout(version)]`
  decode rejection). Aura does not run the sender's OKM and holds no layout
  knowledge; the only contract between the ends is the op byte + byte
  streams. (This supersedes an earlier draft that put a layout version in
  the frame header — wrong because it semantically coupled ends that are
  deliberately unrelated.)
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

### 4. Receiver declaration: `#[kv_storage]` derive

The receiver is declared, not hand-wired. An empty struct annotated with
`#[kv_storage(...)]` declares only its namespace (which app/prefix it
serves). The derive generates **no data methods** (no put/get/scan — there
is no row type to encode) and exactly one execution method
(`exec`/`receive`): take frame → prepend declared prefix → plain engine
execution → fill back scan results.

This pins three things at once: prefix source (declared, not scattered
config), executor shape (macro-generated, not hand-assembled), and
impossibility of prefix escape (the handle is bound to the prefix at
construction; crossing namespaces is not expressible — physical separation,
not naming filters). Same discipline as `#[kv_subscribe]`: the annotation
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
host one `#[kv_storage]` executor per application, each with its own
declared prefix and its own sender-side ns dictionary behind it.

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
escape the prefix it does not hold (the `#[kv_storage]` handle is bound
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
`#[kv_storage]` executor's surface is exactly one method — frame in, results
out; who called it and through which channel is the caller's business.
Read correlation (which response answers which request) lives on the
sender side of the executor, same as write. A TCP + postcard client is
the reference example; it is an example, not part of the contract.
Transport diversity exists only on the two outsides of the executor and
never leaks into it.

## Consequences

- Krystallizer's storage config becomes four-way: mock / fjall / slatedb /
  virtual(→Aura). OKM semantic layers (Table/index/reduce/events) unchanged.
- Aura gains a Storage Actor hosting `#[kv_storage]` executors: one declared
  instance per application (one declared prefix each), frames arrive from
  VirtualStorage backends or realm events; same-machine callers connect
  in-process.
- The frame format is minimal and stable (`op + bytes`); it is NOT a
  compatibility surface between OKM instances — only between a sender's
  trait boundary and a receiver's engine. Changing OKM encoding requires no
  frame changes.
- Write-side seriality is the receiver engine's ordinary single-writer
  behavior, unchanged by remoteness.
