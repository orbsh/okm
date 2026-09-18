//! Dynamic-segment value types (ADR-0012). The dynamic segment's frames
//! are self-describing: each opens with one value-type byte. Declared
//! fields do NOT use this — their types live in the compile-time
//! `FieldDesc` table; this vocabulary exists for fields that appear at
//! run time and must be decoded without a schema.
//!
//! Framing is CBOR-derived, not CBOR: only the major-type pattern is
//! taken — a type nibble opens each frame — and struct encoding rides
//! the same scheme. CBOR's major types include lists and maps; with the
//! field-name dictionary OKM needs only the list: a map IS an n-TLV
//! list (id, type, len, value), structure information living in the
//! values, not in a per-document schema.
//!
//! Tag discipline mirrors [`crate::model::wrappers::EnumTag`]: explicit values,
//! stable under later insertion (append-only growth; never renumber —
//! stored bytes reference these numbers forever).

/// Value types of the dynamic segment. One byte per frame header.
///
/// Integers: signless framing — the caller's Rust type decides width;
/// the wire stores the value big-endian and the reader casts. `Array`
/// is a count-prefixed list of self-describing frames (each element
/// re-carries its type); nested objects use `Obj` with the same
/// reserved contract (added when nesting ships — the variant exists so
/// stored data can already carry the tag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjValueType {
    /// Unsigned integer, big-endian, width implied by the frame length.
    UInt = 0,
    /// IEEE 754 f64, 8 bytes.
    F64 = 1,
    /// UTF-8 string, `len` bytes.
    Str = 2,
    /// Raw bytes, `len` bytes.
    Bytes = 3,
    /// One byte: 0 = false, non-zero = true.
    Bool = 4,
    /// Zero-length frame (len must be 0).
    Null = 5,
    /// Array: `len` counts the ELEMENT count prefix (u32 BE) plus the
    /// concatenated self-describing element frames.
    Array = 6,
    /// Nested object (reserved for the nesting milestone — frames carry
    /// the same shape as the top-level dynamic segment).
    Obj = 7,
    /// Signed integer, big-endian two's complement, width implied by the
    /// frame length (sign-extended on read).
    Int = 8,
}

impl ObjValueType {
    /// Wire byte → type. Unknown tags are normal input for a dynamic
    /// reader (a newer writer stored something it cannot see) — return
    /// `None`, never panic.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::UInt),
            1 => Some(Self::F64),
            2 => Some(Self::Str),
            3 => Some(Self::Bytes),
            4 => Some(Self::Bool),
            5 => Some(Self::Null),
            6 => Some(Self::Array),
            7 => Some(Self::Obj),
            8 => Some(Self::Int),
            _ => None,
        }
    }

    pub fn to_byte(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_and_unknown_is_none() {
        for b in 0u8..=8 {
            let t = ObjValueType::from_byte(b).expect("0-8 are defined");
            assert_eq!(t.to_byte(), b);
        }
        assert_eq!(ObjValueType::from_byte(9), None);
        assert_eq!(ObjValueType::from_byte(0xFF), None);
    }

    #[test]
    fn tags_are_stable_contract() {
        // Explicit byte values, hex-locked: stored frames reference these
        // forever (EnumTag discipline — append-only, never renumber).
        assert_eq!(ObjValueType::UInt.to_byte(), 0);
        assert_eq!(ObjValueType::F64.to_byte(), 1);
        assert_eq!(ObjValueType::Str.to_byte(), 2);
        assert_eq!(ObjValueType::Bytes.to_byte(), 3);
        assert_eq!(ObjValueType::Bool.to_byte(), 4);
        assert_eq!(ObjValueType::Null.to_byte(), 5);
        assert_eq!(ObjValueType::Array.to_byte(), 6);
        assert_eq!(ObjValueType::Obj.to_byte(), 7);
        assert_eq!(ObjValueType::Int.to_byte(), 8);
    }
}
