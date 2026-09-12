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
- **Read path** (`get`/`scan_suffix`) needs a response: it rides the unified
  call model (`ctx.invoke()`, oneshot fill-back, declared fast-call). No
  second waiting mechanism.
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

### 5. Multi-tenant prefix, per ADR-0002's key shape

`app_id/tenant_id` is NOT a namespace layer (ns is compile-time, u16,
dictionary-in-code; tenants are runtime data). The physical key on the
receiver's engine is a pure concatenation — receiver bytes first, sender
bytes after, order never adjusted:

```text
[app_id][tenant_id][ ns 2B ][ sender's key payload ... ]
 └── receiver prefix ──┘  └────── sender bytes, untouched ──────┘
```

The receiver knows only `[app_id][tenant_id]` — its declared prefix. The
ns segment is the sender's: the receiver prepends its prefix to whatever
the frame carries and never parses what follows; ns bytes pass through
opaque. Knowledge asymmetry is the isolation mechanism: the receiver
cannot route between tenants it cannot see past, and the sender cannot
escape the prefix it does not hold (the `#[kv_storage]` handle is bound
to the prefix at construction). ADR-0002's "if tenants exist" clause is
thereby **promoted from sketch to adopted mechanism** (with the segment
order corrected: receiver bytes lead, not ns); its rejection of
per-tenant outer isolation was premised on "no user-programmable query
surface", which the dynamic-execution mode changes — this ADR is the
revision trigger. The ns dictionary discipline itself (compile-time,
append-only, u16) is untouched — ns keeps its in-sender-keyspace
meaning; the receiver prefix lives outside it.

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
Transport diversity exists only on the two outsides of the executor and
never leaks into it.

## Consequences

- Krystallizer's storage config becomes four-way: mock / fjall / slatedb /
  virtual(→Aura). OKM semantic layers (Table/index/reduce/events) unchanged.
- Aura gains a Storage Actor hosting `#[kv_storage]` executors: one declared
  instance per application (prefix = app_id/tenant_id), frames arrive from
  VirtualStorage backends or realm events; same-machine callers connect
  in-process.
- The frame format is minimal and stable (`op + bytes`); it is NOT a
  compatibility surface between OKM instances — only between a sender's
  trait boundary and a receiver's engine. Changing OKM encoding requires no
  frame changes.
- Write-side seriality is the receiver engine's ordinary single-writer
  behavior, unchanged by remoteness.
