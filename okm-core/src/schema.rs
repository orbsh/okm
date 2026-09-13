//! Structured schema export (dynamic codec foundation, ADR-0010 PLAN):
//! the declaration rendered as machine-readable data, not prose.
//!
//! `describe()` is for humans; this module is for code that has no
//! compile-time access to the Rust structs — Python/Steel Actors encode
//! and decode bytes from `TableSchema` alone. Same source (`FIELDS`,
//! `HOT_WIDTH`, `LAYOUT_VERSION`), same discipline as `describe()`: the
//! declaration IS the schema, nothing is duplicated here.
//!
//! The serde forms (feature `schema-serde`) exist so the schema crosses
//! language borders as JSON/whatever; the struct itself is plain data.

use crate::field::FieldType;
use crate::index::{Row, PRIMARY_SLOT};
use crate::key::KeyEncode;

/// One field's placement: a key field is addressed by static offset; a
/// payload field is either hot (static offset inside the hot segment) or
/// cold (TLV tag). `offset` is the byte position within its segment; for
/// cold fields it is the tag number and `width` is 0 (dynamic).
#[derive(Clone, Debug, PartialEq)]
pub struct FieldSchema {
    pub name: String,
    pub ty: FieldType,
    /// Byte width (0 for variable-length kinds: Str, VarInt).
    pub width: usize,
    /// Byte offset within the key (key fields) or the hot segment
    /// (hot payload fields). Zero for cold fields.
    pub offset: usize,
    /// TLV tag for cold payload fields (declaration index); `None` for
    /// key and hot fields.
    pub tag: Option<u8>,
}

/// The complete machine-readable declaration of one table.
#[derive(Clone, Debug, PartialEq)]
pub struct TableSchema {
    pub key_len: usize,
    pub key_fields: Vec<FieldSchema>,
    pub layout_version: u8,
    /// Byte width of the hot payload segment (header excluded).
    pub hot_width: usize,
    /// Payload header is `[version u8][hot_len u16 BE]`.
    pub payload_header_len: usize,
    pub hot_fields: Vec<FieldSchema>,
    pub cold_fields: Vec<FieldSchema>,
}

impl TableSchema {
    /// Export the declaration of `<K, R>` as structured data.
    pub fn of<K: KeyEncode, R: Row<Key = K>>() -> Self {
        let mut key_fields = Vec::new();
        let mut off = 0usize;
        for f in <K as KeyEncode>::FIELDS {
            key_fields.push(FieldSchema {
                name: f.name.to_string(),
                ty: f.ty,
                width: f.width,
                offset: off,
                tag: None,
            });
            off += f.width;
        }
        let key_len = K::KEY_LEN;
        debug_assert_eq!(off, key_len, "key FIELDS widths must sum to KEY_LEN");

        let mut hot_fields = Vec::new();
        let mut cold_fields = Vec::new();
        let mut hot_off = 0usize;
        for (fi, f) in <R as Row>::FIELDS.iter().enumerate() {
            if f.width > 0 {
                hot_fields.push(FieldSchema {
                    name: f.name.to_string(),
                    ty: f.ty,
                    width: f.width,
                    offset: hot_off,
                    tag: None,
                });
                hot_off += f.width;
            } else {
                cold_fields.push(FieldSchema {
                    name: f.name.to_string(),
                    ty: f.ty,
                    width: 0,
                    offset: 0,
                    tag: Some(fi as u8),
                });
            }
        }
        Self {
            key_len,
            key_fields,
            layout_version: R::LAYOUT_VERSION,
            hot_width: R::HOT_WIDTH,
            payload_header_len: 3, // [version u8][hot_len u16 BE]
            hot_fields,
            cold_fields,
        }
    }

    /// Slot 0 = primary (ADR-0005); index slots live outside this schema
    /// (they are derived state, rebuilt from rows).
    pub const PRIMARY_SLOT: u8 = PRIMARY_SLOT;
}

#[cfg(feature = "schema-serde")]
mod serde_impls {
    use super::{FieldSchema, TableSchema};
    use crate::field::FieldType;
    use serde::{Deserialize, Serialize};

    impl Serialize for FieldType {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            // Tagged: variant name + payload where present — the dynamic
            // side needs the payload (Quant precision, Offset base).
            match self {
                FieldType::U8 => s.serialize_unit_variant("FieldType", 0, "U8"),
                FieldType::U16 => s.serialize_unit_variant("FieldType", 1, "U16"),
                FieldType::U32 => s.serialize_unit_variant("FieldType", 2, "U32"),
                FieldType::U64 => s.serialize_unit_variant("FieldType", 3, "U64"),
                FieldType::FixedBytes => {
                    s.serialize_unit_variant("FieldType", 4, "FixedBytes")
                }
                FieldType::Str => s.serialize_unit_variant("FieldType", 5, "Str"),
                FieldType::VarInt => s.serialize_unit_variant("FieldType", 6, "VarInt"),
                FieldType::Quant(p) => {
                    s.serialize_newtype_variant("FieldType", 7, "Quant", p)
                }
                FieldType::Enum => s.serialize_unit_variant("FieldType", 8, "Enum"),
                FieldType::Offset(b) => {
                    s.serialize_newtype_variant("FieldType", 9, "Offset", b)
                }
            }
        }
    }

    impl<'de> Deserialize<'de> for FieldType {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            enum Repr {
                U8,
                U16,
                U32,
                U64,
                FixedBytes,
                Str,
                VarInt,
                Quant(u32),
                Enum,
                Offset(i64),
            }
            Ok(match Repr::deserialize(d)? {
                Repr::U8 => FieldType::U8,
                Repr::U16 => FieldType::U16,
                Repr::U32 => FieldType::U32,
                Repr::U64 => FieldType::U64,
                Repr::FixedBytes => FieldType::FixedBytes,
                Repr::Str => FieldType::Str,
                Repr::VarInt => FieldType::VarInt,
                Repr::Quant(p) => FieldType::Quant(p),
                Repr::Enum => FieldType::Enum,
                Repr::Offset(b) => FieldType::Offset(b),
            })
        }
    }

    impl Serialize for FieldSchema {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            #[derive(Serialize)]
            struct Repr<'a> {
                name: &'a str,
                ty: &'a FieldType,
                width: usize,
                offset: usize,
                tag: &'a Option<u8>,
            }
            Repr {
                name: &self.name,
                ty: &self.ty,
                width: self.width,
                offset: self.offset,
                tag: &self.tag,
            }
            .serialize(s)
        }
    }

    impl<'de> Deserialize<'de> for FieldSchema {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            struct Repr {
                name: String,
                ty: FieldType,
                width: usize,
                offset: usize,
                tag: Option<u8>,
            }
            let r = Repr::deserialize(d)?;
            Ok(Self {
                name: r.name,
                ty: r.ty,
                width: r.width,
                offset: r.offset,
                tag: r.tag,
            })
        }
    }

    impl Serialize for TableSchema {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            #[derive(Serialize)]
            struct Repr<'a> {
                key_len: usize,
                key_fields: &'a [FieldSchema],
                layout_version: u8,
                hot_width: usize,
                payload_header_len: usize,
                hot_fields: &'a [FieldSchema],
                cold_fields: &'a [FieldSchema],
            }
            Repr {
                key_len: self.key_len,
                key_fields: &self.key_fields,
                layout_version: self.layout_version,
                hot_width: self.hot_width,
                payload_header_len: self.payload_header_len,
                hot_fields: &self.hot_fields,
                cold_fields: &self.cold_fields,
            }
            .serialize(s)
        }
    }

    impl<'de> Deserialize<'de> for TableSchema {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            struct Repr {
                key_len: usize,
                key_fields: Vec<FieldSchema>,
                layout_version: u8,
                hot_width: usize,
                payload_header_len: usize,
                hot_fields: Vec<FieldSchema>,
                cold_fields: Vec<FieldSchema>,
            }
            let r = Repr::deserialize(d)?;
            Ok(Self {
                key_len: r.key_len,
                key_fields: r.key_fields,
                layout_version: r.layout_version,
                hot_width: r.hot_width,
                payload_header_len: r.payload_header_len,
                hot_fields: r.hot_fields,
                cold_fields: r.cold_fields,
            })
        }
    }

}
