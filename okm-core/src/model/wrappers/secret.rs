//! `Secret` — a declared field whose bytes must not leak by accident (ADR-0069).
//!
//! Wire layout is exactly `Bytes`: the same variable-length TLV frame, the same
//! `FieldType::Bytes` in the FieldDesc table. This type is a SPELLING plus three
//! guard rails, because none of them can be attached to a bare `Bytes`:
//!
//! - `Debug` prints the length, never the bytes — a `dbg!(row)` or a `tracing`
//!   field is the cheapest way to leak a secret, and the derive is what makes it
//!   the easiest.
//! - `Secret` implements no key codec (`KeyEncode`), so it cannot be a key field.
//! - The derive refuses `#[ok_index(fields(…))]` naming it: an index entry's KEY
//!   carries the field's encoded value, so an indexed secret is a sorted copy of
//!   the secret.
//!
//! The bytes are the CIPHERTEXT. The engine holds no key, by design (ADR-0069
//! Decision 5): the application encrypts with AAD bound to `ns ‖ pkey ‖ field
//! name`, and a ciphertext moved to another row then fails to open.
//!
//! A secret is not encrypted at rest by this crate. Do not store a password here
//! — store its Argon2id hash (also a `Secret`, because a hash is still not
//! something to print or index).

/// Variable-length bytes that `Debug` refuses to show and that no index may name.
///
/// Memory form is `Vec<u8>`; wire form is the `Bytes` frame. Access is explicit
/// (`expose` / `into_inner`) rather than `Deref`, so a stray `{:?}` reach for the
/// bytes has to be written on purpose.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(pub Vec<u8>);

impl Secret {
    pub fn new(v: Vec<u8>) -> Self {
        Secret(v)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The bytes, borrowed — the ciphertext (or the hash), never a plaintext the
    /// engine could use.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The bytes, owned.
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The length is not a secret in any useful sense, and it is what makes a
        // log line diagnosable ("this row's token is 0 bytes").
        write!(f, "Secret(<redacted>, {} bytes)", self.0.len())
    }
}

impl From<Vec<u8>> for Secret {
    fn from(v: Vec<u8>) -> Self {
        Secret(v)
    }
}

impl From<&[u8]> for Secret {
    fn from(v: &[u8]) -> Self {
        Secret(v.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_print_the_bytes() {
        let secret = Secret::new(vec![0xde, 0xad, 0xbe, 0xef]);
        let printed = format!("{secret:?}");
        assert_eq!(printed, "Secret(<redacted>, 4 bytes)");
        assert!(!printed.contains("de"), "{printed}");
        assert_eq!(secret.expose(), &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(secret.into_inner(), vec![0xde, 0xad, 0xbe, 0xef]);
    }
}