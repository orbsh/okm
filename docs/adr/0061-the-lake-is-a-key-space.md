# ADR-0061: The lake is a key space, not a directory

Date: 2026-11-11
Status: RULED & LANDED 2026-11-11 (library side). The verbs in the binary still take paths; §4
says why that is the remaining half. §4's gap is closed 2026-11-12 — see §7.

## 1. Context

Every lake verb was written against `Path`: the drain created `data/`, wrote Parquet into it and
renamed it into place; the Delta log was `read_dir` over `_delta_log`; vacuum and status stat'ed
files. That was right while the consumer was a person running a CLI over a copied directory.

The first real producer is a service that holds its own state and wants that state somewhere a
warehouse can read it (k10r ADR-0014) — and "somewhere" means a bucket. Teaching each verb a
second shape would have meant two implementations of the same protocol, which is exactly the
thing this crate exists to avoid.

## 2. Decision: one trait, two stores, keys instead of paths

**`store::LakeStore`** is the whole of a lake's storage: `get`, `put`, `put_new`, `list`,
`head`, `delete`, `describe`. Keys are relative to the lake root and use `/`:
`<table>/data/<file>.parquet`, `<table>/_delta_log/<version>.json`,
`watermark-<consumer>-<table>.txt`.

Two implementations:

- **`LocalFs`** — a directory. `put` writes beside the target and renames; `put_new` opens with
  `create_new`, so the claim is atomic for the same reason the rename was.
- **`S3`** — a bucket, through `object_store` with an owned current-threaded runtime behind a
  SYNCHRONOUS facade. The verbs are batch steps; a lake that forced a runtime into its callers
  would have made the storage layer everyone's problem. `put_new` is `PutMode::Create` (a
  conditional PUT), and a server that answers 412 instead is treated as "not mine" too: the
  caller's question is only whether this version was claimed.

The trait has no directory concept and no rename because an object store has neither. What it
does have is what the protocol actually uses: **an object is written once and never edited**, and
a version is claimed by a write that fails if the key exists. Both stores show that property to
the Delta writer, so `write_actions` is one function.

## 3. What moved with it

- **`delta` speaks `(store, table)`.** `read_log` lists the log prefix; `file_summary` reads
  bytes, not a file; `Addition.path` is a key and `Addition.size` is stated by the writer (a
  store has no `stat`). The path-shaped verbs keep their names with an `_at` suffix — `commit_files_at`,
  `files_in_order_at` — and are three lines: build a `LocalFs` from the parent directory, and
  call the store form. One implementation, two faces.
- **`publish::replace_table`** — the producer's entry point: put the Parquet file, commit a
  version that REPLACES the previous one's files, delete what it replaced. A service that owns
  its state publishes one file per table per snapshot; it never needs the change-log path.
- **`snapshot::parquet_io`** gained `batch_bytes`, `export_parquet_with_variant_bytes` and
  `export_reduces_bytes`: what a bucket needs is bytes, and a caller holding a store should not
  have to route a file through a temporary directory to get them.

## 4. What did not move

The binary still takes paths: `okm-lake --root <log> --out <dir>`, `condense`, `vacuum`,
`status`. Porting those to a url is mechanical now (each verb's fs call sites are the ones
already replaced in `delta`), but it changes the CLI's arguments and its tests, and no consumer
needs it yet: a drain from a change log runs where the log is, and a producer publishes through
`publish::replace_table`. The gap is stated rather than hidden — `--out` does not accept
`s3://` today.

## 5. Verification

`okm-lake/tests/s3_test.rs` runs the whole publish path against a real bucket and skips when
none is configured (`OKM_S3_URL`, `OKM_S3_ENDPOINT`, `OKM_S3_ACCESS_KEY`, `OKM_S3_SECRET`). It
was run against a local rustfs: two publishes produced versions 0 and 1, the replaced object left
the store, the log listed one live file, the bytes came back with the Parquet magic, and a
create-only write on an existing version answered "not mine". The local store keeps its 101 tests
unchanged.

## 6. Consequences

- `object_store`, `tokio` and `futures` join this crate's dependencies. They are the cost of
  speaking S3 without hand-rolling SigV4, and they sit behind `store`, so nothing above the
  trait sees them.
- A caller inside an async runtime must call the store verbs from `spawn_blocking`: the facade
  owns a runtime, and nesting one inside another is not a thing tokio allows.
- Deleting a replaced file immediately (in `publish`) is deliberate: a publisher mirrors live
  state. A producer that wants history keeps the versions instead and vacuums on its own
  schedule.
## 7. Amendment (2026-11-12): the binary takes a url

§4's gap is closed. `--out` (also spelled `--store` / `--lake`) parses into `store::LakeUrl` —
one arm per form, no bucket contacted — and drain, condense, vacuum and status read and write
through `LakeStore`. What `delta` had already done, the rest of the crate now does: the
path-shaped verbs are adapters (`*_at`), and a directory is one store among two. The drain's
position, its manifests and its Parquet all go through the store, so a bucket is a lake for
every verb rather than for `publish` alone. Verification: `okm-lake/tests/cli_test.rs` runs the
binary end to end on a local lake, and `cli.rs` asserts each address form without a network.
