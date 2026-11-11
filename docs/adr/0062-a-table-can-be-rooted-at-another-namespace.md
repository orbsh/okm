# ADR-0062: A table can be rooted at another namespace

Date: 2026-11-11
Status: RULED & LANDED 2026-11-11

## 1. Context

A restore has one job that a store scan cannot do: put a lake's table somewhere other than the namespace its declaration names. The application that owns the destination creates an empty ns and asks for the rows there (k10r ADR-0015: partial import, migration, one-way analytics handoff); the lake's table keeps the name it was published under.

The namespace is not a property of the store handle — it is the first field of the key header, `[part][ns 2B][…]`, and every verb of a `Collection` reaches the key through one function, `header()`. So the question was never "how do we write under another ns" but "who owns the 2 bytes".

## 2. Decision: one field on the handle

`Collection` carries `ns_override: Option<u16>`. `with_ns(store, ns)` builds a handle with it set, `rebased(&self, ns)` returns this handle's engine with it set, and `header()` prepends `ns.to_be_bytes()` instead of the declaration's `NS_PREFIX`. `ns()` answers the effective namespace, or `None` for a declaration that carries no ns field at all (some test documents do not).

`Graph` gets the same field (`[u8; 2]`), the same two constructors, and `has_edges()`. A graph moves as a WHOLE — nodes, edges, faces, community numbers — because an edge's endpoints are `[ns 2B]` references into a node table: an edge restored beside nodes that stayed behind points at nothing, and no reader can tell that from a dangling row.

`has_edges()` exists for the restore's first question (is this namespace untouched?). Nodes and edges share one ns, so "nothing is here" is not a question about the node table alone.

## 3. What moved with it

- **`header()` is public**, and `parquet_io::import_parquet` reads its vocabulary prefix from the collection rather than from the declaration. That is the whole of the mapping: a restore into ns 900 interns the file's labels into ns 900's numbering, because ids are local and labels are the identity.
- **`parquet_io::read_batches(bytes)`** and **`import_parquet_batch(collection, batch)`**: a lake hands over bytes, so a restore must not route them through a temp file to satisfy a `&Path` signature. `import_parquet` is now the reader loop over the batch form, not a second implementation.

## 4. What does not change

A caller that does not ask for another namespace sees byte-identical keys: `None` is the declaration's prefix, and nothing else about encoding, indexing, faces or reduces consults `ns_override`. There is no new write path to keep in step — the same `header()` feeds put, scan, index and fold, which is what made a field the right size for this change.

## 5. Rejected

- **A second declaration type per destination ns** — the same rows, two truths about what they are; every future field would have to be added twice.
- **Rewriting the ns inside the store layer** — a store would have to parse every key's header to know what it is moving, which is exactly the coupling between engine and schema this crate exists to prevent.
- **A `Vec<u8>` / variable-length ns prefix** — the ns is 2 bytes in the header the engine already commits to; making it variable makes every key's width depend on a string.
- **Moving nodes without edges** (or edges without nodes) — see §2: a reference into an absent table reads as data, not as an error.
- **A `restore` verb on `Collection`** — a restore is a lake-shaped plan over many tables with a refusal rule, not a per-table operation; it lives in the application (k10r), where the emptiness policy belongs.

## 6. Consequences

The restore is now a mapping (`lake table → ns`) instead of a special build, and a full restore is the same code with an empty mapping. Two limits are accepted: a mapped restore's vocabulary ids differ from the source's (labels are the identity; ids are local), and the ns a table lands in must be decided by the caller — okm does not keep a registry of "where this table went".