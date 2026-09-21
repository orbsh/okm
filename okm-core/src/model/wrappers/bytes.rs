//! `Bytes` — declared-field spelling for variable-length raw bytes
//! (blob payloads: hashes, ciphertext, serialized frames).
//!
//! Wire layout: the same cold TLV frame as `String` minus the UTF-8
//! constraint — `[tag][len varint][raw bytes]`, the frame header emitted
//! by the shared TLV loop. This type is a SPELLING, not a codec: it lets
//! the derive classify the field as `FieldType::Bytes` from the type name
//! alone, replacing the retired `Vec<u8>` spelling (whose list-shaped name
//! misdescribed the byte-string semantics — see PLAN, scalar list fields).
//!
//! Not to be confused with the dynamic layer's escape hatch: a declared
//! `Bytes` field is part of the document schema (name in the field-name
//! dictionary, `FieldType::Bytes` in the FieldDesc table); the
//! `ObjValueType::Bytes` VALUE kind is the untyped dynamic-segment form.

/// Variable-length raw-byte field. Memory form is `Vec<u8>`; wire form is
/// the frame described above.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    pub fn new(v: Vec<u8>) -> Self {
        Bytes(v)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Bytes(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Self {
        Bytes(v.to_vec())
    }
}

impl std::ops::Deref for Bytes {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deref_and_conversions() {
        let b = Bytes::new(vec![1, 2, 3]);
        assert_eq!(b.len(), 3);
        assert!(!b.is_empty());
        assert_eq!(&b[..], &[1, 2, 3]); // Deref<Target = Vec<u8>>
        assert_eq!(Bytes::from(&[9u8][..]), Bytes(vec![9]));
        assert_eq!(Bytes::default(), Bytes(vec![]));
    }
}
