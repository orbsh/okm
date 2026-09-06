# ADR-0001: Edge direction bit niched into the namespace field

Date: 2026-09-06
Status: Accepted

## Context

A bidirectional edge needs a direction marker in its key so forward and reverse entries of the same relationship live under distinct prefixes. The edge header sits at the very front of the key, so its encoding decides whether prefix scanning stays trivial.

Three candidate designs were evaluated:

1. **Standalone direction byte** — `[dir: 1B][ns: 2B][payload]`. Simple, but wastes a whole byte on a value that only needs one bit.
2. **Bit-stream packing** — ns and direction packed into a shared bit stream. Squeezes to minimal bits, but the 2-byte/3-byte boundary sharing between header and payload complicates prefix arithmetic and violates the fixed-width, byte-aligned foundation.
3. **Niche into the high bit of the ns u16** — header = `(ns << 1 | dir).to_be_bytes()`, 2 bytes total, FWD = 0, REV = 1.

## Analysis

- KV keys have **no alignment requirement**; key length is byte-level, but that is a language/hardware limit (`&[u8]`, memcmp operate on bytes), not a reason to add a third byte.
- Option 2 yields a 3-byte total with an A/B boundary sharing a byte — prefix scans then cannot treat the header as a fixed 2-byte tag.
- Rust has no `u15` type, so option 3 means the declared type stays `u16` while the actual namespace range is 0–32767. The declaration is one bit wider than the true capacity. Accepted: the alternative (a separate bit-stream header) is structurally worse, and 32768 namespaces is far beyond any realistic count.

## Decision

Niche the direction bit into the top bit of the ns field:

```
header = (ns << 1 | dir).to_be_bytes()   // 2 bytes, BE
ns effective range: 0..=32767
DIR_BIT = 0x8000
```

The declared `u16` therefore overstates capacity by one bit — an accepted, documented discrepancy rather than a structural lie (the alternative designs were strictly worse).
