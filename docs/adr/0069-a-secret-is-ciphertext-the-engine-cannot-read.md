# 0069 — A secret is stored as ciphertext, and the engine does not hold the key

Status: decided 2026-11-17, implemented. Adds one field spelling to `okm-core` and one attribute to the derive; touches no key layout and no value layout.

## Context

An application has to keep credentials it must be able to READ BACK: an API token, a device password replayed to a switch, a TOTP seed. Losing them loses the account; a hash cannot be replayed. So these are stored encrypted, and the question this ADR settles is which side of the boundary holds the key.

Three leaks are on by default in this engine today:

1. **A field can be indexed.** `#[ok_index(fields(…))]` puts the field's encoded value *into the index entry's key* (`KvIndex::entry_key`, `okm-core/src/model/index.rs`), and `includes` puts it in the entry value. An indexed secret is a second copy — ordered, enumerable, and scanned by every range query over that index.
2. **A field is exported.** The lake bridge turns every declared field into a parquet column (`okm-lake/src/bridge.rs`, `build_batch`), so a secret leaves the store as a column.
3. **A field is printed.** `Debug` is derived, so one `dbg!(row)` or one `tracing` field and the plaintext is in a log.

None of these is a mistake in isolation: they are what a field normally does. They are wrong for exactly one kind of field.

## Decision

**1. Secrets are encrypted by the application; okm stores ciphertext.** On the wire a secret is `FieldType::Bytes` — the same variable-length TLV frame `Bytes` already uses. No new field type, no key or value layout change, no layout-generation bump (ADR-0042).

**2. `Secret` is a type, and the guard rails hang on it.** `okm_core::Secret` is a newtype over the same bytes with one difference: `Debug` prints `Secret(<redacted>, N bytes)`. It is the only spelling that carries secret semantics, so the protections below are properties of the type rather than a sentence in a document that the next field violates.

**3. What may enter an index is decided by what the key can be USED as, not by whether the bytes are random.** An index entry's key is readable by anything that can scan it, and an equality probe on that key is an enumeration oracle. Two sides:

*A secret cannot be indexed, and saying so is a compile error.* `#[ok_index(fields(…))]` naming a `Secret` field is refused by the derive, not at run time, and `Secret` implements no key codec, so it cannot be a key field either. The reason is (1) above: an index is a sorted copy, and a sorted copy of a secret is the secret.

*A one-way digest can be indexed, when its preimage cannot be walked.* This is the shape for a credential that is verified BY presentation: the application hashes the presented token (SHA-256; no salt and no stretching, because the input is 128 bits of randomness, not a guessable string), stores the digest as `[u8; 32]` — `FieldType::FixedBytes`, fixed width, so it may sit anywhere in `fields(…)` — and looks a row up by the digest. The digest is not the credential and cannot be turned back into one, so equality on it is not an oracle. A digest of an ENUMERABLE preimage — a six-digit code, a password, a name — is still the value, and stays out of every key.

**4. Export writes ciphertext by default; `#[ok_redact]` writes null instead.** Ciphertext is the useful default — a customer-side consumer decrypts it, and a key rotation re-wraps what is already there. Redaction is per field and explicit, because a column that quietly disappears is harder to notice than a column that is null.

**5. The engine does not hold a key.** No KEK, no DEK, no KMS client, no crypto dependency in `okm-core`. The trust boundary is the process: whatever the application can decrypt, the engine could too, so moving the key inside buys nothing it does not already have.

## Consequences

- The application owns the cipher and the key material: XChaCha20-Poly1305 or AES-GCM, with AAD bound to `ns ‖ pkey ‖ field name`. Binding the row's identity means a ciphertext moved to another row fails to open — the cheapest defence against a copy-paste between tenants, and the same rule the face layout follows (`[ns][slot]` is not decoration).
- Key rotation and crypto-shredding are the application's: re-encrypt the rows, or drop the key and the ciphertext is gone. The engine has nothing to rotate.
- The changelog carries the ciphertext as it carries any value, and replay copies it verbatim — no key needed on the replay path (unlike the engine-held-key form, where replay has to open and re-seal each payload).
- Hashing a credential does not make a stolen store harmless by hiding the credential; it makes the stolen store yield VERIFIERS instead of credentials. That is the layer hashing buys, and it is the one encryption at rest does not: a live process holds the key beside the ciphertext, while a digest has no key to steal. Salt and stretching address the enumerable case — they cost an attacker per guess — and buy nothing where the preimage space cannot be walked. Neither layer touches a process that already holds the plaintext; that one is short TTLs and no logging.
- A secret field cannot be sorted, ranged over, or grouped. That is the point; a query about credentials goes through data that is not the credential — the owner id, the tenant, the label — and never through the secret itself.
- Password *verification* is out of scope and unchanged: a password is stored as an Argon2id hash (a PHC string), which is public-by-design and needs no encryption — only the same refusal to index it, which falls out of storing it as a `Secret`.

## Rejected

- **Envelope encryption inside the engine (a DEK per ns).** Worth it only when the key may not live in the application process, or when "delete one tenant by dropping its key" is the requirement. The cost is the whole chain: sealing at the codec boundary (two persistence writers, the engine and the changelog frames), every read path, replay, export, rotation, and a key-holding store that a crash dump can leak. "Store a password" does not buy that.
- **Deterministic encryption plus an index, for equality lookups.** A deterministic ciphertext in an index key *is* the secret as a lookup key: equality on it is enumeration, which is the leak the encryption was for. SIV does not change this — the leak is the answer, not the primitive. So equality is refused with range, not beside it. A one-way digest is the other side of that line (§3): allowed, because it cannot be replayed as a credential.
- **Storing a password as a reversible ciphertext.** Verification never needs the plaintext; a reversible form only widens the attack surface.
- **Whole-volume or whole-engine encryption as the answer to this requirement.** That is the filesystem's job (LUKS, fscrypt) and it is orthogonal: it protects a stolen disk, not a query, an export, or a log line.

## Tests the implementation owes

- A `Secret` field round-trips byte-for-byte identically to its `Bytes` twin (the layout claim in Decision 1).
- `Debug` on a row holding a secret contains no plaintext byte of it.
- `#[ok_index(fields(secret))]` is a compile error (`okm-core/tests/compilefail/`), and a `Secret` key field does not compile because the type has no key codec.
- A 32-byte digest field is indexable at a position that is not last — the token-verification shape of §3 — and the entry key carries the digest, never the token.
- The lake bridge writes ciphertext for a secret column and null under `#[ok_redact]`, with the column still present.