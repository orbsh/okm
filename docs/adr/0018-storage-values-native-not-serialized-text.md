# ADR-0018: Storage values ARE the engine's value model — serialized text is never a storage representation

Date: 2026-09-20
Status: Accepted. Applies to every okm consumer, not only to the case that produced it.
Related: ADR-0012 (object model: declared fields + dynamic segment), ADR-0010 (§3 the receiver bridge), ADR-0004 (value side)

## Context

A defect found in okm's primary consumer (aura) exposed a rule that was never written down:

- Booth state was stored as **JSON text** — `serde_json::to_vec(value)`, one blob per field.
- okm's tables in that realm were driven through the **raw byte trait** (`VirtualStorage`) implemented *over* that JSON-typed store: on write, okm's bytes were **base64**-encoded into a JSON string, because the container in the middle could only carry text.

The three taxes follow mechanically, but the causal chain is the real finding: **there was never a reason to encode bytes as base64.** There was a text container in the middle of the storage path, and a text-only container forces a text representation of bytes. base64 is not a design decision; it is the signature of a misplaced container.

The taxes, named so they are recognizable elsewhere:

1. **Binary fidelity is lost at a boundary that had no business existing.** A round trip through text is exact only if every writer remembers to encode; one that forgets writes a silently corrupt value.
2. **Size.** base64 inflates by a third; JSON text adds its own quoting and escaping; a `Vec<u8>` through `serde_json` becomes an array of numbers, roughly 3–4× its bytes.
3. **The value model is destroyed.** A stored blob is opaque to the engine: no field-level index, no scan by field, no reduce, no comparison, no typed path, no schema evolution. okm's value model exists exactly to avoid this — `DynamicValue` carries `Bytes(Vec<u8>)` natively, and the document API (`put_document` / `get_document`) writes declared fields through the typed path and everything else into the dynamic segment in one call.

## Decision

### 1. The storage currency is okm's value model

`DynamicValue` (declared fields via the typed path, the rest in the dynamic segment) for object-shaped data, raw bytes for hand-rolled layouts. Nothing else is stored.

### 2. Serialized text is never a storage representation, unless the layer physically cannot hold anything else

JSON, YAML, TOML, XML, CSV — texts are **interface** currencies. A text encoding may cross an interface (an LLM-facing frame, a script-facing API, a config file); it may not be what the engine holds. A text blob in storage is a defect to be fixed by **removing the text container**, never by optimizing the text encoding.

### 3. base64 is a boundary artifact, never a storage format

It appears only where a text-only container must carry bytes. Its presence is a signal that a text container sits where it does not belong. This ADR exists because that signal was ignored once.

### 4. A consumer binds its tables to an okm engine, not to its own state store

A consumer keeps whatever currency its API needs — aura keeps JSON, because its booths are polyglot scripts and an LLM is a party to the protocol — and converts **once, at the seam**, through a single `serde_json::Value ↔ DynamicValue` module. The tables themselves bind to an engine that speaks okm's currency.

Worked example, to be implemented in aura: `StoreAsVirtual` (the base64 adapter) is deleted; the mq tables bind to `FjallStore` over the same fjall database in their own keyspace; booth state becomes one document per instance, so state is binary-native and readable by okm's own machinery.

## Alternatives considered

- **Keep the JSON-typed store as the storage currency** (the status quo). Rejected: the three taxes above, and every okm table forced through a container that cannot hold what it stores.
- **Change the booth API's currency to `DynamicValue`** (no JSON anywhere in the system). Rejected for now: the API's consumers are polyglot scripts and an LLM, whose natural currency is JSON — that is precisely where JSON belongs. If the API later needs binary fidelity beyond JSON, that is a separate decision, and the storage layer no longer waits on it.
- **Swap the text encoding for a better one** (CBOR/postcard instead of JSON, or a denser base64 alphabet). Rejected: optimizing the wrong container.
- **Per-field rows carrying hand-encoded `DynamicValue` frames.** Rejected: it reaches into okm's internal frame encoding instead of using the public document API.
- **Migrate the existing bytes.** Rejected in favour of a clean break (pre-production, no real data on disk). If that ever stops being true, migration is a read-only reader rebuilding everything through the normal write path — a restore path, never a second write channel.

## Consequences

- **aura**: `StoreAsVirtual` deleted; mq tables bound to `FjallStore` (same database, own keyspace); booth state represented as a document per instance; existing mq/state bytes are discarded (clean break), which the ADR states openly rather than hiding behind a migration.
- **Consumers generally**: the value model is the contract. A stored text blob is a review finding even when it appears to work.
- **probe**: unaffected — it holds no storage at all (ADR-0010 §7).
- **Enforcement is a criterion, not a taste**: the question to ask is "what does the engine hold?", not "what does the wire carry?".