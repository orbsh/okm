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
use crate::index::{Document, PRIMARY_SLOT};
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
    /// Default value for version migration: a payload written by an
    /// older layout lacks this field, the dynamic reader fills it from
    /// here (mirrors the Rust decoder's `#[ok_default]`/Default rule).
    /// `None` = the export carries no literal default (the Rust side
    /// always has one — expression or `Default::default()` — but only
    /// literal expressions are exportable data).
    pub default: Option<DefaultValue>,
}

/// Exportable default literal. Deliberately narrow: the dynamic reader
/// has no Rust type context, so defaults travel as plain data.
#[derive(Clone, Debug, PartialEq)]
pub enum DefaultValue {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(String),
    /// Zero-filled byte string of schema width (FixedBytes default).
    ZeroBytes(usize),
}

/// The dynamic segment's frame vocabulary (ADR-0012). Mirrors
/// `okm_core::wrappers::ObjValueType` tag bytes — the dynamic reader
/// dispatches on these.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjValueTypeSchema {
    UInt,
    Int,
    F64,
    Str,
    Bytes,
    Bool,
    Null,
    Array,
    Obj,
}

impl ObjValueTypeSchema {
    /// Wire tag byte.
    pub fn tag(&self) -> u8 {
        match self {
            Self::UInt => 0,
            Self::Int => 8,
            Self::F64 => 1,
            Self::Str => 2,
            Self::Bytes => 3,
            Self::Bool => 4,
            Self::Null => 5,
            Self::Array => 6,
            Self::Obj => 7,
        }
    }
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
    /// Slot map (ADR-0012 final allocation): the dynamic reader needs to
    /// know where the dynamic segment and the dictionary live, and where
    /// declared index/reduce slots begin.
    pub slots: SlotMap,
}

/// Find a field's const default by name; converts to the owned form.
fn lookup_default(
    defaults: &'static [(&'static str, crate::field::DefaultValueConst)],
    name: &str,
) -> Option<DefaultValue> {
    defaults.iter().find(|(n, _)| *n == name).map(|(_, d)| match d {
        crate::field::DefaultValueConst::U64(x) => DefaultValue::U64(*x),
        crate::field::DefaultValueConst::I64(x) => DefaultValue::I64(*x),
        crate::field::DefaultValueConst::F64(x) => DefaultValue::F64(*x),
        crate::field::DefaultValueConst::Bool(x) => DefaultValue::Bool(*x),
        crate::field::DefaultValueConst::Str(x) => DefaultValue::Str(x.to_string()),
    })
}

/// Fixed-role slot numbers (ADR-0012); declared index/reduce slots start
/// at `declared_base` in declaration order.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotMap {
    pub primary: u8,
    pub dynamic: u8,
    pub dict_id: u8,
    pub dict_name: u8,
    pub edge_fwd: u8,
    pub edge_rev: u8,
    pub declared_base: u8,
}

impl TableSchema {
    /// Export the declaration of `<K, R>` as structured data.
    pub fn of<K: KeyEncode, R: Document<Key = K>>() -> Self {
        let mut key_fields = Vec::new();
        let mut off = 0usize;
        for f in <K as KeyEncode>::FIELDS {
            key_fields.push(FieldSchema {
                name: f.name.to_string(),
                ty: f.ty,
                width: f.width,
                offset: off,
                tag: None,
                default: None,
            });
            off += f.width;
        }
        let key_len = K::KEY_LEN;
        debug_assert_eq!(off, key_len, "key FIELDS widths must sum to KEY_LEN");

        let mut hot_fields = Vec::new();
        let mut cold_fields = Vec::new();
        let mut hot_off = 0usize;
        for (fi, f) in <R as Document>::FIELDS.iter().enumerate() {
            if f.width > 0 {
                hot_fields.push(FieldSchema {
                    name: f.name.to_string(),
                    ty: f.ty,
                    width: f.width,
                    offset: hot_off,
                    tag: None,
                    default: lookup_default(<R as Document>::DEFAULTS, f.name),
                });
                hot_off += f.width;
            } else {
                cold_fields.push(FieldSchema {
                    name: f.name.to_string(),
                    ty: f.ty,
                    width: 0,
                    offset: 0,
                    tag: Some(fi as u8),
                    default: lookup_default(<R as Document>::DEFAULTS, f.name),
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
            slots: SlotMap {
                primary: crate::index::PRIMARY_SLOT,
                dynamic: crate::index::DYNAMIC_SLOT,
                dict_id: crate::index::DICT_ID_SLOT,
                dict_name: crate::index::DICT_NAME_SLOT,
                edge_fwd: crate::index::EDGE_FWD_SLOT,
                edge_rev: crate::index::EDGE_REV_SLOT,
                declared_base: crate::index::DECLARED_SLOT_BASE,
            },
        }
    }

    /// Slot 0 = primary (ADR-0005); index slots live outside this schema
    /// (they are derived state, rebuilt from documents).
    pub const PRIMARY_SLOT: u8 = PRIMARY_SLOT;
}

#[cfg(feature = "schema-serde")]
mod serde_impls {
    use super::{DefaultValue, FieldSchema, ObjValueTypeSchema, SlotMap, TableSchema};
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
                FieldType::Bytes => s.serialize_unit_variant("FieldType", 10, "Bytes"),
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

    impl Serialize for ObjValueTypeSchema {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_u8(self.tag())
        }
    }

    impl<'de> Deserialize<'de> for ObjValueTypeSchema {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let b = u8::deserialize(d)?;
            match b {
                0 => Ok(Self::UInt),
                1 => Ok(Self::F64),
                2 => Ok(Self::Str),
                3 => Ok(Self::Bytes),
                4 => Ok(Self::Bool),
                5 => Ok(Self::Null),
                6 => Ok(Self::Array),
                7 => Ok(Self::Obj),
                8 => Ok(Self::Int),
                _ => Err(serde::de::Error::custom("unknown obj value type")),
            }
        }
    }

    impl Serialize for SlotMap {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeStruct;
            let mut st = s.serialize_struct("SlotMap", 7)?;
            st.serialize_field("primary", &self.primary)?;
            st.serialize_field("dynamic", &self.dynamic)?;
            st.serialize_field("dict_id", &self.dict_id)?;
            st.serialize_field("dict_name", &self.dict_name)?;
            st.serialize_field("edge_fwd", &self.edge_fwd)?;
            st.serialize_field("edge_rev", &self.edge_rev)?;
            st.serialize_field("declared_base", &self.declared_base)?;
            st.end()
        }
    }

    impl<'de> Deserialize<'de> for SlotMap {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            struct Repr {
                primary: u8,
                dynamic: u8,
                dict_id: u8,
                dict_name: u8,
                edge_fwd: u8,
                edge_rev: u8,
                declared_base: u8,
            }
            let r = Repr::deserialize(d)?;
            Ok(Self {
                primary: r.primary,
                dynamic: r.dynamic,
                dict_id: r.dict_id,
                dict_name: r.dict_name,
                edge_fwd: r.edge_fwd,
                edge_rev: r.edge_rev,
                declared_base: r.declared_base,
            })
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
                default: &'a Option<DefaultValue>,
            }
            Repr {
                name: &self.name,
                ty: &self.ty,
                width: self.width,
                offset: self.offset,
                tag: &self.tag,
                default: &self.default,
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
                default: Option<DefaultValue>,
            }
            let r = Repr::deserialize(d)?;
            Ok(Self {
                name: r.name,
                ty: r.ty,
                width: r.width,
                offset: r.offset,
                tag: r.tag,
                default: r.default,
            })
        }
    }

    impl Serialize for DefaultValue {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            match self {
                DefaultValue::U64(x) => s.serialize_newtype_variant("DefaultValue", 0, "U64", x),
                DefaultValue::I64(x) => s.serialize_newtype_variant("DefaultValue", 1, "I64", x),
                DefaultValue::F64(x) => s.serialize_newtype_variant("DefaultValue", 2, "F64", x),
                DefaultValue::Bool(x) => s.serialize_newtype_variant("DefaultValue", 3, "Bool", x),
                DefaultValue::Str(x) => s.serialize_newtype_variant("DefaultValue", 4, "Str", x),
                DefaultValue::ZeroBytes(n) => {
                    s.serialize_newtype_variant("DefaultValue", 5, "ZeroBytes", n)
                }
            }
        }
    }

    impl<'de> Deserialize<'de> for DefaultValue {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            enum Repr {
                U64(u64),
                I64(i64),
                F64(f64),
                Bool(bool),
                Str(String),
                ZeroBytes(usize),
            }
            match Repr::deserialize(d)? {
                Repr::U64(x) => Ok(DefaultValue::U64(x)),
                Repr::I64(x) => Ok(DefaultValue::I64(x)),
                Repr::F64(x) => Ok(DefaultValue::F64(x)),
                Repr::Bool(x) => Ok(DefaultValue::Bool(x)),
                Repr::Str(x) => Ok(DefaultValue::Str(x)),
                Repr::ZeroBytes(x) => Ok(DefaultValue::ZeroBytes(x)),
            }
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
                slots: &'a SlotMap,
            }
            Repr {
                key_len: self.key_len,
                key_fields: &self.key_fields,
                layout_version: self.layout_version,
                hot_width: self.hot_width,
                payload_header_len: self.payload_header_len,
                hot_fields: &self.hot_fields,
                cold_fields: &self.cold_fields,
                slots: &self.slots,
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
                slots: SlotMap,
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
                slots: r.slots,
            })
        }
    }

}
