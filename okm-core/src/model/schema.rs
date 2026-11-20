//! Structured schema export (dynamic codec foundation, ADR-0010 PLAN):
//! the declaration rendered as machine-readable data, not prose.
//!
//! `describe()` is for humans; this module is for code that has no
//! compile-time access to the Rust structs — Python/Steel Actors encode
//! and decode bytes from `CollectionSchema` alone. Same source (`FIELDS`,
//! `HOT_WIDTH`, `LAYOUT_VERSION`), same discipline as `describe()`: the
//! declaration IS the schema, nothing is duplicated here.
//!
//! The serde forms (feature `schema-serde`) exist so the schema crosses
//! language borders as JSON/whatever; the struct itself is plain data.

use crate::model::field::FieldType;
use crate::model::index::{Document, PRIMARY_SLOT};
use crate::model::key::KeyEncode;

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
    /// `Vector` element-count contract (`#[ok_len(N)]` / the dynamic
    /// reader's declared expectation): a decode-time assertion, not a
    /// wire constraint — the frame carries its own count. `None` =
    /// unrestricted (the dynamic-mode default).
    pub expect_len: Option<usize>,
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
pub struct CollectionSchema {
    /// The table's declared name (`#[ok_name]`, ADR-0034): what a lake table is
    /// called, and what a segment header carries so the name outlives the
    /// directory it was written in (ADR-0056 §7). Empty means the declaration
    /// names none, and an export refuses that.
    pub name: String,
    pub key_len: usize,
    pub key_fields: Vec<FieldSchema>,
    /// The collection's `[ns 2B]` header as the declaration carries it
    /// (`#[ok_ns]`). Empty = the declaration names no ns (layout-only).
    /// The ns and the schema travel as a PAIR: assembly compares them and
    /// rejects a disagreement instead of overriding one with the other
    /// (ADR-0025, ADR-0017 §4).
    pub ns_prefix: Vec<u8>,
    pub layout_version: u8,
    /// Byte width of the hot payload segment (header excluded).
    pub hot_width: usize,
    pub hot_fields: Vec<FieldSchema>,
    pub cold_fields: Vec<FieldSchema>,
    /// The member collections of a GRAPH's edge set (ADR-0017 §4): the ns of
    /// every node collection the graph admits. Empty for an ordinary document
    /// collection, and empty also means "declares no set".
    pub members: Vec<u16>,
    /// `(ns, key width)` for every member collection — the table an edge key is
    /// parsed by, because an edge carries no length prefix per endpoint
    /// (ADR-0067). It is the runtime half of the compile-time `NodeKinds`, and a
    /// dynamic writer needs it to build and read a key at all: without the width,
    /// `[src][pred][dst]` has no boundary.
    pub member_widths: Vec<(u16, usize)>,
    /// Slot map (ADR-0012 final allocation): the dynamic reader needs to
    /// know where the dynamic segment and the vocabulary live, and where
    /// declared index/reduce slots begin.
    pub slots: SlotMap,
    /// The declared structural faces (ADR-0048), in declaration order. These
    /// live outside the primary table, so nothing about them is visible in a
    /// payload — this list is the only description of them the store can be
    /// checked against.
    pub indexes: Vec<IndexSchema>,
    /// `R::FACES`: the fingerprint of `indexes`, as the store's meta row carries
    /// it. Exported so an outside reader can compare without re-deriving.
    pub faces: Vec<u8>,
    /// Field names whose export column is NULL (`#[ok_redact]`, ADR-0069 §4).
    /// A secret field is absent from this list: its column carries the ciphertext
    /// it already holds, which is the useful default — a reader on the other side
    /// owns the key.
    pub redacted: Vec<String>,
}

/// One declared face, as the export carries it (ADR-0048): the same six facts
/// the compile-time fingerprint is taken over.
///
/// Indexes and lead faces share this shape deliberately — they are both "a
/// declared face over this collection", and the difference (payload segment vs
/// identity segment, ADR-0054) is already in `slot`. [`IndexSchema::kind`] reads
/// it back out instead of a seventh field repeating it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSchema {
    pub name: String,
    pub slot: u16,
    pub fields: Vec<String>,
    pub includes: Vec<String>,
    pub func: String,
    pub deprecated: bool,
}

impl IndexSchema {
    /// Which declaration kind owns this face — read off the slot family, never
    /// stored beside it (ADR-0054).
    ///
    /// Matters to a reader because `fields` means different things per kind: a
    /// payload field to encode for an `Index`, a key field to slice for a `Lead`.
    pub fn kind(&self) -> Option<crate::model::faces::FaceKind> {
        crate::model::faces::FaceKind::of(self.slot)
    }
}

/// `#[ok_len(N)]` lookup over a document's FIELD_CONTRACTS table.
fn expect_len_of(contracts: &'static [(&'static str, usize)], name: &str) -> Option<usize> {
    contracts.iter().find(|(n, _)| *n == name).map(|(_, l)| *l)
}

/// Find a field's const default by name; converts to the owned form.
fn lookup_default(
    defaults: &'static [(&'static str, crate::model::field::DefaultValueConst)],
    name: &str,
) -> Option<DefaultValue> {
    defaults
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| match d {
            crate::model::field::DefaultValueConst::U64(x) => DefaultValue::U64(*x),
            crate::model::field::DefaultValueConst::I64(x) => DefaultValue::I64(*x),
            crate::model::field::DefaultValueConst::F64(x) => DefaultValue::F64(*x),
            crate::model::field::DefaultValueConst::Bool(x) => DefaultValue::Bool(*x),
            crate::model::field::DefaultValueConst::Str(x) => DefaultValue::Str(x.to_string()),
        })
}

/// Fixed-role slots and declared-slot segment bases (ADR-0016: slot =
/// 4-bit segment + 12-bit counter); declared index/reduce counters start
/// at 1 inside their segments.
///
/// Where each family of entries lives in a collection's key space. A host that
/// reads or writes keys itself (a remote binding replaying a plan, a dynamic
/// reader) needs this table, so it is part of the exported contract — not an
/// internal convenience.
///
/// The four vocabulary slots are two pairs, each an ID face plus a name face;
/// every pair ALSO owns one allocation cursor, keyed at its ID face's bare
/// prefix — `[ns][field_vocab_id]` with no id bytes, the same for the predicate
/// pair (ADR-0030's face-level singleton, ADR-0032's cursor). That key is
/// DERIVED, so it is not a field here: `okm_core::obj_vocab::cursor_key` builds
/// it. A host-side writer that allocates ids itself reads and writes that
/// entry; going around it hands out ids the store has already used.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotMap {
    pub primary: u16,
    pub dynamic: u16,
    pub field_vocab_id: u16,
    pub field_vocab_name: u16,
    /// Predicate vocabulary (ADR-0017 §3): a graph's predicates, a node
    /// collection's kind tokens — its own pair, never a band of 0x02/0x03.
    pub predicate_vocab_id: u16,
    pub predicate_vocab_name: u16,
    pub declared_index_base: u16,
    pub declared_reduce_base: u16,
    /// `0x6nnn` base: lead faces count from here (ADR-0054), in their own family
    /// so a declaration cannot take a number an index would have used.
    pub declared_lead_base: u16,
}

impl SlotMap {
    /// The slot numbers every document declaration resolves to (ADR-0030): the
    /// assembly point does not choose them, the numbering does. Named once so a
    /// reader of the map (the capture filter, ADR-0056 §2) takes them from the
    /// declaration instead of re-typing the constants.
    pub fn standard() -> Self {
        Self {
            primary: crate::model::index::PRIMARY_SLOT,
            dynamic: crate::model::index::DYNAMIC_SLOT,
            field_vocab_id: crate::model::index::FIELD_VOCAB_ID_SLOT,
            field_vocab_name: crate::model::index::FIELD_VOCAB_NAME_SLOT,
            predicate_vocab_id: crate::model::index::PREDICATE_VOCAB_ID_SLOT,
            predicate_vocab_name: crate::model::index::PREDICATE_VOCAB_NAME_SLOT,
            declared_index_base: crate::model::index::DECLARED_SLOT_BASE,
            declared_reduce_base: crate::model::index::REDUCE_SLOT_BASE,
            declared_lead_base: crate::model::index::LEAD_SLOT_BASE,
        }
    }
}

impl CollectionSchema {
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
                expect_len: None,
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
                    expect_len: None,
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
                    // Vector fields export the #[ok_len] contract for the
                    // dynamic reader; other cold kinds have none.
                    expect_len: expect_len_of(<R as Document>::FIELD_CONTRACTS, f.name),
                });
            }
        }
        Self {
            name: R::TABLE_NAME.to_string(),
            key_len,
            key_fields,
            ns_prefix: R::NS_PREFIX.to_vec(),
            layout_version: R::LAYOUT_VERSION,
            hot_width: R::HOT_WIDTH,
            hot_fields,
            cold_fields,
            members: Vec::new(),
            member_widths: Vec::new(),
            slots: SlotMap::standard(),
            indexes: <R as Document>::INDEX_DECLS
                .iter()
                .map(|d| IndexSchema {
                    name: d.name.to_string(),
                    slot: d.slot,
                    fields: d.fields.iter().map(|f| (*f).to_string()).collect(),
                    includes: d.includes.iter().map(|f| (*f).to_string()).collect(),
                    func: d.func.to_string(),
                    deprecated: d.deprecated,
                })
                .collect(),
            faces: <R as Document>::FACES.to_vec(),
            redacted: <R as Document>::REDACTED
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        }
    }

    /// Slot 0 = primary (ADR-0005); index slots live outside this schema
    /// (they are derived state, rebuilt from documents).
    pub const PRIMARY_SLOT: u16 = PRIMARY_SLOT;

    /// Export a GRAPH declaration (ADR-0017 §4): the edge collection's schema
    /// plus its member set — the ns of every collection the graph admits.
    /// The member set is the one half of the declaration a runtime writer
    /// cannot derive from the payload layout and must enforce at write; it
    /// rides the schema as DATA (ADR-0025: mode is a property of the writer),
    /// never as a stored slot.
    pub fn of_graph<E: crate::model::graph::GraphEncode>() -> Self {
        use crate::model::graph::{Members, NodeKinds};
        let mut schema = Self::of::<crate::model::graph::EdgeKey, E>();
        schema.members = <E::Nodes as NodeKinds>::NS_VALUES.to_vec();
        schema.member_widths = Members::of::<E::Nodes>().pairs().to_vec();
        schema
    }
}

#[cfg(feature = "schema-serde")]
mod serde_impls {
    use super::{
        CollectionSchema, DefaultValue, FieldSchema, IndexSchema, ObjValueTypeSchema, SlotMap,
    };
    use crate::model::field::FieldType;
    use serde::{Deserialize, Serialize};

    /// Plain data: a flat struct of name/strings/bool, so the derive is the
    /// honest serialization here (the hand-written impls above exist because
    /// those types carry tagged variants).
    impl Serialize for IndexSchema {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            #[derive(serde::Serialize)]
            struct Repr<'a> {
                name: &'a str,
                slot: u16,
                fields: &'a [String],
                includes: &'a [String],
                func: &'a str,
                deprecated: bool,
            }
            Repr {
                name: &self.name,
                slot: self.slot,
                fields: &self.fields,
                includes: &self.includes,
                func: &self.func,
                deprecated: self.deprecated,
            }
            .serialize(s)
        }
    }

    impl<'de> Deserialize<'de> for IndexSchema {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(serde::Deserialize)]
            struct Repr {
                name: String,
                slot: u16,
                #[serde(default)]
                fields: Vec<String>,
                #[serde(default)]
                includes: Vec<String>,
                #[serde(default)]
                func: String,
                #[serde(default)]
                deprecated: bool,
            }
            let r = Repr::deserialize(d)?;
            Ok(Self {
                name: r.name,
                slot: r.slot,
                fields: r.fields,
                includes: r.includes,
                func: r.func,
                deprecated: r.deprecated,
            })
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
            let mut st = s.serialize_struct("SlotMap", 10)?;
            st.serialize_field("primary", &self.primary)?;
            st.serialize_field("dynamic", &self.dynamic)?;
            st.serialize_field("field_vocab_id", &self.field_vocab_id)?;
            st.serialize_field("field_vocab_name", &self.field_vocab_name)?;
            st.serialize_field("predicate_vocab_id", &self.predicate_vocab_id)?;
            st.serialize_field("predicate_vocab_name", &self.predicate_vocab_name)?;
            st.serialize_field("declared_index_base", &self.declared_index_base)?;
            st.serialize_field("declared_reduce_base", &self.declared_reduce_base)?;
            st.serialize_field("declared_lead_base", &self.declared_lead_base)?;
            st.end()
        }
    }

    impl<'de> Deserialize<'de> for SlotMap {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            struct Repr {
                primary: u16,
                dynamic: u16,
                #[serde(alias = "dict_id")]
                field_vocab_id: u16,
                #[serde(alias = "dict_name")]
                field_vocab_name: u16,
                /// Schemas written before the predicate vocabulary existed carry
                /// no pair; the fixed faces are the fallback numbers. (No alias
                /// for the retired `kind_id`/`kind_name` spelling: "kind" is not
                /// this slot's name since ADR-0017 — the edge's type is the
                /// predicate, and a schema either carries the pair or defaults.)
                #[serde(default = "default_predicate_vocab_id")]
                predicate_vocab_id: u16,
                #[serde(default = "default_predicate_vocab_name")]
                predicate_vocab_name: u16,
                declared_index_base: u16,
                declared_reduce_base: u16,
                /// Exported before lead faces existed: the family base is fixed
                /// by ADR-0030, so defaulting it is exact, not a guess.
                #[serde(default = "default_lead_base")]
                declared_lead_base: u16,
            }
            fn default_lead_base() -> u16 {
                crate::model::index::LEAD_SLOT_BASE
            }
            fn default_predicate_vocab_id() -> u16 {
                crate::model::index::PREDICATE_VOCAB_ID_SLOT
            }
            fn default_predicate_vocab_name() -> u16 {
                crate::model::index::PREDICATE_VOCAB_NAME_SLOT
            }
            let r = Repr::deserialize(d)?;
            Ok(Self {
                primary: r.primary,
                dynamic: r.dynamic,
                field_vocab_id: r.field_vocab_id,
                field_vocab_name: r.field_vocab_name,
                predicate_vocab_id: r.predicate_vocab_id,
                predicate_vocab_name: r.predicate_vocab_name,
                declared_index_base: r.declared_index_base,
                declared_reduce_base: r.declared_reduce_base,
                declared_lead_base: r.declared_lead_base,
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
                expect_len: &'a Option<usize>,
            }
            Repr {
                name: &self.name,
                ty: &self.ty,
                width: self.width,
                offset: self.offset,
                tag: &self.tag,
                default: &self.default,
                expect_len: &self.expect_len,
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
                expect_len: Option<usize>,
            }
            let r = Repr::deserialize(d)?;
            Ok(Self {
                name: r.name,
                ty: r.ty,
                width: r.width,
                offset: r.offset,
                tag: r.tag,
                default: r.default,
                expect_len: r.expect_len,
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

    impl Serialize for CollectionSchema {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            #[derive(Serialize)]
            struct Repr<'a> {
                name: &'a str,
                key_len: usize,
                key_fields: &'a [FieldSchema],
                ns_prefix: &'a [u8],
                layout_version: u8,
                hot_width: usize,
                hot_fields: &'a [FieldSchema],
                cold_fields: &'a [FieldSchema],
                members: &'a [u16],
                member_widths: &'a [(u16, usize)],
                slots: &'a SlotMap,
                indexes: &'a [IndexSchema],
                faces: &'a [u8],
                redacted: &'a [String],
            }
            Repr {
                name: &self.name,
                key_len: self.key_len,
                key_fields: &self.key_fields,
                ns_prefix: &self.ns_prefix,
                layout_version: self.layout_version,
                hot_width: self.hot_width,
                hot_fields: &self.hot_fields,
                cold_fields: &self.cold_fields,
                members: &self.members,
                member_widths: &self.member_widths,
                slots: &self.slots,
                indexes: &self.indexes,
                faces: &self.faces,
                redacted: &self.redacted,
            }
            .serialize(s)
        }
    }

    impl<'de> Deserialize<'de> for CollectionSchema {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            struct Repr {
                /// Sidecars written before the name travelled carry none; an
                /// empty name reads as "unnamed", which the export refuses.
                #[serde(default)]
                name: String,
                key_len: usize,
                key_fields: Vec<FieldSchema>,
                /// Writers that predate the pair carry no ns; assembly then
                /// has one half only and cannot check a disagreement.
                #[serde(default)]
                ns_prefix: Vec<u8>,
                layout_version: u8,
                hot_width: usize,
                hot_fields: Vec<FieldSchema>,
                cold_fields: Vec<FieldSchema>,
                /// Schemas written before the member set existed carry none.
                #[serde(default)]
                members: Vec<u16>,
                /// Schemas written before edges dropped their length prefix
                /// carry no widths; such a schema can read its own tables but
                /// cannot key a graph edge (ADR-0067).
                #[serde(default)]
                member_widths: Vec<(u16, usize)>,
                slots: SlotMap,
                /// Schemas written before faces were exported carry none. An
                /// empty list means "not described", which reads as unknown
                /// faces — never as "this table declares no index".
                #[serde(default)]
                indexes: Vec<IndexSchema>,
                #[serde(default)]
                faces: Vec<u8>,
                /// Sidecars written before redaction existed carry none — the
                /// same reading as a table that declares none.
                #[serde(default)]
                redacted: Vec<String>,
            }
            let r = Repr::deserialize(d)?;
            Ok(Self {
                name: r.name,
                key_len: r.key_len,
                key_fields: r.key_fields,
                ns_prefix: r.ns_prefix,
                layout_version: r.layout_version,
                hot_width: r.hot_width,
                hot_fields: r.hot_fields,
                cold_fields: r.cold_fields,
                members: r.members,
                member_widths: r.member_widths,
                slots: r.slots,
                indexes: r.indexes,
                faces: r.faces,
                redacted: r.redacted,
            })
        }
    }
}

/// Where two declarations disagree, named the way a human has to act on it (ADR-0074 §4).
///
/// One difference, not a diff: a shape change usually makes several, and the first one is
/// what tells the reader what happened. The alternative — "the bytes differ" — is true and
/// useless at the hour it gets read.
pub fn difference(stored: &CollectionSchema, build: &CollectionSchema) -> String {
    fn field_diff(what: &str, a: &[FieldSchema], b: &[FieldSchema]) -> Option<String> {
        if a.len() != b.len() {
            return Some(format!(
                "{what}: stored {} field(s), this build declares {}",
                a.len(),
                b.len()
            ));
        }
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            let at = format!("{what}[{i}]");
            if x.name != y.name {
                return Some(format!(
                    "{at}.name: stored {:?}, build {:?}",
                    x.name, y.name
                ));
            }
            if x.ty != y.ty {
                return Some(format!("{at}.ty: stored {:?}, build {:?}", x.ty, y.ty));
            }
            if x.width != y.width {
                return Some(format!("{at}.width: stored {}, build {}", x.width, y.width));
            }
            if x.offset != y.offset {
                return Some(format!(
                    "{at}.offset: stored {}, build {}",
                    x.offset, y.offset
                ));
            }
            if x.tag != y.tag {
                return Some(format!("{at}.tag: stored {:?}, build {:?}", x.tag, y.tag));
            }
            if x.default != y.default {
                return Some(format!(
                    "{at}.default: stored {:?}, build {:?}",
                    x.default, y.default
                ));
            }
            if x.expect_len != y.expect_len {
                return Some(format!(
                    "{at}.expect_len: stored {:?}, build {:?}",
                    x.expect_len, y.expect_len
                ));
            }
        }
        None
    }

    let simple: Option<String> = None
        .or_else(|| {
            (stored.name != build.name)
                .then(|| format!("name: stored {:?}, build {:?}", stored.name, build.name))
        })
        .or_else(|| {
            (stored.ns_prefix != build.ns_prefix).then(|| {
                format!(
                    "ns_prefix: stored {:?}, build {:?}",
                    stored.ns_prefix, build.ns_prefix
                )
            })
        })
        .or_else(|| {
            (stored.layout_version != build.layout_version).then(|| {
                format!(
                    "layout_version: stored {}, build {}",
                    stored.layout_version, build.layout_version
                )
            })
        })
        .or_else(|| {
            (stored.members != build.members).then(|| {
                format!(
                    "members: stored {:?}, build {:?}",
                    stored.members, build.members
                )
            })
        })
        .or_else(|| {
            (stored.member_widths != build.member_widths).then(|| {
                format!(
                    "member_widths: stored {:?}, build {:?}",
                    stored.member_widths, build.member_widths
                )
            })
        })
        .or_else(|| {
            (stored.faces != build.faces)
                .then(|| format!("faces: stored {:?}, build {:?}", stored.faces, build.faces))
        })
        .or_else(|| {
            (stored.redacted != build.redacted).then(|| {
                format!(
                    "redacted: stored {:?}, build {:?}",
                    stored.redacted, build.redacted
                )
            })
        })
        .or_else(|| {
            (stored.slots != build.slots).then(|| {
                format!(
                    "slot map: stored {:?}, build {:?}",
                    stored.slots, build.slots
                )
            })
        });

    simple
        .or_else(|| field_diff("key_fields", &stored.key_fields, &build.key_fields))
        .or_else(|| field_diff("hot_fields", &stored.hot_fields, &build.hot_fields))
        .or_else(|| field_diff("cold_fields", &stored.cold_fields, &build.cold_fields))
        .or_else(|| {
            (stored.key_len != build.key_len).then(|| {
                format!(
                    "key_len: stored {}, build {}",
                    stored.key_len, build.key_len
                )
            })
        })
        .or_else(|| {
            (stored.hot_width != build.hot_width).then(|| {
                format!(
                    "hot_width: stored {}, build {}",
                    stored.hot_width, build.hot_width
                )
            })
        })
        .or_else(|| {
            if stored.indexes.len() != build.indexes.len() {
                return Some(format!(
                    "indexes: stored {}, build {}",
                    stored.indexes.len(),
                    build.indexes.len()
                ));
            }
            for (i, (x, y)) in stored.indexes.iter().zip(&build.indexes).enumerate() {
                if x != y {
                    return Some(format!(
                        "indexes[{i}]: stored {:?}, build {:?}",
                        x.name, y.name
                    ));
                }
            }
            None
        })
        .unwrap_or_else(|| "the declarations differ in a way this report does not name".to_string())
}

// ---------------------------------------------------------------------------
// The binary form (ADR-0074 §2)
// ---------------------------------------------------------------------------

/// Format byte of the schema record itself — NOT the table's `layout_version`.
///
/// Two different questions: `layout_version` says which payload shape a store was written
/// in (ADR-0042); this byte says which encoding the *description* is in. A reader that does
/// not know the number cannot decode far enough to compare anything, which is why "just
/// compare the bytes" cannot replace it — and why the two failures a human actually cares
/// about (another build's encoder vs. a declaration that disagrees with the store) stay
/// distinguishable in the error.
pub const SCHEMA_FORMAT: u8 = 1;

impl CollectionSchema {
    /// This declaration as bytes, in the crate's own codec family: `put_len` for every
    /// length, big-endian for fixed-width numbers, and no serde anywhere on the path.
    pub fn encode(&self) -> Vec<u8> {
        bin::encode(self)
    }

    /// The inverse. Panics on bytes this build could not have written (ADR-0040) instead of
    /// parsing forgivingly: a truncated field list or an unknown type tag is a bug or a
    /// foreign build, never data to be guessed at.
    pub fn decode(bytes: &[u8]) -> Self {
        bin::decode(bytes)
    }
}

mod bin {
    use super::{CollectionSchema, DefaultValue, FieldSchema, FieldType, IndexSchema, SlotMap};
    use crate::model::field::Elem;
    use crate::model::wrappers::wire::{put_len, take_len};

    fn put_str(buf: &mut Vec<u8>, s: &str) {
        put_len(buf, s.len());
        buf.extend_from_slice(s.as_bytes());
    }

    fn put_bytes(buf: &mut Vec<u8>, b: &[u8]) {
        put_len(buf, b.len());
        buf.extend_from_slice(b);
    }

    fn put_u16(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_be_bytes());
    }

    fn field_type_tag(ty: &FieldType) -> u8 {
        match ty {
            FieldType::U8 => 0,
            FieldType::U16 => 1,
            FieldType::U32 => 2,
            FieldType::U64 => 3,
            FieldType::FixedBytes => 4,
            FieldType::Str => 5,
            FieldType::Bytes => 6,
            FieldType::VarInt => 7,
            FieldType::Quant(_) => 8,
            FieldType::Vocab(_) => 9,
            FieldType::Offset(_) => 10,
            FieldType::Uuid4 => 11,
            FieldType::Uuid7 => 12,
            FieldType::Vector(_) => 13,
        }
    }

    fn elem_tag(e: Elem) -> u8 {
        match e {
            Elem::F32 => 0,
            Elem::U32 => 1,
            Elem::I32 => 2,
            Elem::U64 => 3,
            Elem::I64 => 4,
            Elem::Str => 5,
        }
    }

    fn encode_field(buf: &mut Vec<u8>, f: &FieldSchema) {
        put_str(buf, &f.name);
        buf.push(field_type_tag(&f.ty));
        match f.ty {
            FieldType::Quant(scale) => buf.extend_from_slice(&scale.to_be_bytes()),
            FieldType::Vocab(face) => put_u16(buf, face),
            FieldType::Offset(base) => buf.extend_from_slice(&base.to_be_bytes()),
            FieldType::Vector(e) => buf.push(elem_tag(e)),
            _ => {}
        }
        put_len(buf, f.width);
        put_len(buf, f.offset);
        match f.tag {
            Some(t) => {
                buf.push(1);
                buf.push(t);
            }
            None => buf.push(0),
        }
        match &f.default {
            Some(d) => {
                buf.push(1);
                encode_default(buf, d);
            }
            None => buf.push(0),
        }
        match f.expect_len {
            Some(n) => {
                buf.push(1);
                put_len(buf, n);
            }
            None => buf.push(0),
        }
    }

    fn encode_default(buf: &mut Vec<u8>, d: &DefaultValue) {
        match d {
            DefaultValue::U64(v) => {
                buf.push(0);
                buf.extend_from_slice(&v.to_be_bytes());
            }
            DefaultValue::I64(v) => {
                buf.push(1);
                buf.extend_from_slice(&v.to_be_bytes());
            }
            DefaultValue::F64(v) => {
                buf.push(2);
                buf.extend_from_slice(&v.to_bits().to_be_bytes());
            }
            DefaultValue::Bool(v) => {
                buf.push(3);
                buf.push(u8::from(*v));
            }
            DefaultValue::Str(v) => {
                buf.push(4);
                put_str(buf, v);
            }
            DefaultValue::ZeroBytes(n) => {
                buf.push(5);
                put_len(buf, *n);
            }
        }
    }

    fn encode_index(buf: &mut Vec<u8>, i: &IndexSchema) {
        put_str(buf, &i.name);
        put_u16(buf, i.slot);
        put_u16(buf, i.fields.len() as u16);
        for f in &i.fields {
            put_str(buf, f);
        }
        put_u16(buf, i.includes.len() as u16);
        for f in &i.includes {
            put_str(buf, f);
        }
        put_str(buf, &i.func);
        buf.push(u8::from(i.deprecated));
    }

    pub(super) fn encode(s: &CollectionSchema) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        buf.push(super::SCHEMA_FORMAT);
        buf.push(s.layout_version);
        put_str(&mut buf, &s.name);
        put_bytes(&mut buf, &s.ns_prefix);
        put_len(&mut buf, s.key_len);
        put_u16(&mut buf, s.key_fields.len() as u16);
        for f in &s.key_fields {
            encode_field(&mut buf, f);
        }
        put_len(&mut buf, s.hot_width);
        put_u16(&mut buf, s.hot_fields.len() as u16);
        for f in &s.hot_fields {
            encode_field(&mut buf, f);
        }
        put_u16(&mut buf, s.cold_fields.len() as u16);
        for f in &s.cold_fields {
            encode_field(&mut buf, f);
        }
        put_u16(&mut buf, s.members.len() as u16);
        for m in &s.members {
            put_u16(&mut buf, *m);
        }
        put_u16(&mut buf, s.member_widths.len() as u16);
        for (ns, w) in &s.member_widths {
            put_u16(&mut buf, *ns);
            put_len(&mut buf, *w);
        }
        for v in [
            s.slots.primary,
            s.slots.dynamic,
            s.slots.field_vocab_id,
            s.slots.field_vocab_name,
            s.slots.predicate_vocab_id,
            s.slots.predicate_vocab_name,
            s.slots.declared_index_base,
            s.slots.declared_reduce_base,
            s.slots.declared_lead_base,
        ] {
            put_u16(&mut buf, v);
        }
        put_u16(&mut buf, s.indexes.len() as u16);
        for i in &s.indexes {
            encode_index(&mut buf, i);
        }
        put_bytes(&mut buf, &s.faces);
        put_u16(&mut buf, s.redacted.len() as u16);
        for r in &s.redacted {
            put_str(&mut buf, r);
        }
        buf
    }

    struct Cursor<'a> {
        b: &'a [u8],
    }

    impl<'a> Cursor<'a> {
        fn new(b: &'a [u8]) -> Self {
            Self { b }
        }

        fn u8(&mut self) -> u8 {
            let v = *self.b.first().expect("schema: truncated at a tag byte");
            self.b = &self.b[1..];
            v
        }

        fn u16(&mut self) -> u16 {
            assert!(self.b.len() >= 2, "schema: truncated at a u16");
            let v = u16::from_be_bytes([self.b[0], self.b[1]]);
            self.b = &self.b[2..];
            v
        }

        fn len(&mut self) -> usize {
            let (v, n) = take_len(self.b).expect("schema: truncated at a length");
            self.b = &self.b[n..];
            v
        }

        fn str(&mut self) -> String {
            let n = self.len();
            assert!(self.b.len() >= n, "schema: truncated inside a string");
            let s = String::from_utf8(self.b[..n].to_vec()).expect("schema: name is not UTF-8");
            self.b = &self.b[n..];
            s
        }

        fn bytes(&mut self) -> Vec<u8> {
            let n = self.len();
            assert!(self.b.len() >= n, "schema: truncated inside a byte string");
            let v = self.b[..n].to_vec();
            self.b = &self.b[n..];
            v
        }

        fn opt(&mut self) -> bool {
            match self.u8() {
                0 => false,
                1 => true,
                other => panic!("schema: option byte {other} is not 0 or 1"),
            }
        }

        fn field_type(&mut self) -> FieldType {
            match self.u8() {
                0 => FieldType::U8,
                1 => FieldType::U16,
                2 => FieldType::U32,
                3 => FieldType::U64,
                4 => FieldType::FixedBytes,
                5 => FieldType::Str,
                6 => FieldType::Bytes,
                7 => FieldType::VarInt,
                8 => {
                    let n = self.b.len();
                    assert!(n >= 4, "schema: truncated Quant payload");
                    let v = u32::from_be_bytes([self.b[0], self.b[1], self.b[2], self.b[3]]);
                    self.b = &self.b[4..];
                    FieldType::Quant(v)
                }
                9 => FieldType::Vocab(self.u16()),
                10 => {
                    let n = self.b.len();
                    assert!(n >= 8, "schema: truncated Offset payload");
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&self.b[..8]);
                    self.b = &self.b[8..];
                    FieldType::Offset(i64::from_be_bytes(w))
                }
                11 => FieldType::Uuid4,
                12 => FieldType::Uuid7,
                13 => FieldType::Vector(match self.u8() {
                    0 => Elem::F32,
                    1 => Elem::U32,
                    2 => Elem::I32,
                    3 => Elem::U64,
                    4 => Elem::I64,
                    5 => Elem::Str,
                    other => panic!("schema: vector element tag {other} is not a declared kind"),
                }),
                other => panic!("schema: field type tag {other} is not a declared kind"),
            }
        }

        fn default_value(&mut self) -> DefaultValue {
            match self.u8() {
                0 => {
                    let n = self.b.len();
                    assert!(n >= 8, "schema: truncated u64 default");
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&self.b[..8]);
                    self.b = &self.b[8..];
                    DefaultValue::U64(u64::from_be_bytes(w))
                }
                1 => {
                    let n = self.b.len();
                    assert!(n >= 8, "schema: truncated i64 default");
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&self.b[..8]);
                    self.b = &self.b[8..];
                    DefaultValue::I64(i64::from_be_bytes(w))
                }
                2 => {
                    let n = self.b.len();
                    assert!(n >= 8, "schema: truncated f64 default");
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&self.b[..8]);
                    self.b = &self.b[8..];
                    DefaultValue::F64(f64::from_bits(u64::from_be_bytes(w)))
                }
                3 => DefaultValue::Bool(self.u8() != 0),
                4 => DefaultValue::Str(self.str()),
                5 => DefaultValue::ZeroBytes(self.len()),
                other => panic!("schema: default tag {other} is not a declared kind"),
            }
        }

        fn field(&mut self) -> FieldSchema {
            let name = self.str();
            let ty = self.field_type();
            let width = self.len();
            let offset = self.len();
            let tag = if self.opt() { Some(self.u8()) } else { None };
            let default = if self.opt() {
                Some(self.default_value())
            } else {
                None
            };
            let expect_len = if self.opt() { Some(self.len()) } else { None };
            FieldSchema {
                name,
                ty,
                width,
                offset,
                tag,
                default,
                expect_len,
            }
        }

        fn fields(&mut self) -> Vec<FieldSchema> {
            let n = self.u16() as usize;
            (0..n).map(|_| self.field()).collect()
        }

        fn index(&mut self) -> IndexSchema {
            let name = self.str();
            let slot = self.u16();
            let n = self.u16() as usize;
            let fields: Vec<String> = (0..n).map(|_| self.str()).collect();
            let n = self.u16() as usize;
            let includes: Vec<String> = (0..n).map(|_| self.str()).collect();
            let func = self.str();
            let deprecated = self.u8() != 0;
            IndexSchema {
                name,
                slot,
                fields,
                includes,
                func,
                deprecated,
            }
        }
    }

    pub(super) fn decode(bytes: &[u8]) -> CollectionSchema {
        let mut c = Cursor::new(bytes);
        let fmt = c.u8();
        assert_eq!(
            fmt,
            super::SCHEMA_FORMAT,
            "schema: format {fmt}, this build writes {}",
            super::SCHEMA_FORMAT
        );
        let layout_version = c.u8();
        let name = c.str();
        let ns_prefix = c.bytes();
        let key_len = c.len();
        let key_fields = c.fields();
        let hot_width = c.len();
        let hot_fields = c.fields();
        let cold_fields = c.fields();
        let n = c.u16() as usize;
        let members: Vec<u16> = (0..n).map(|_| c.u16()).collect();
        let n = c.u16() as usize;
        let member_widths: Vec<(u16, usize)> = (0..n).map(|_| (c.u16(), c.len())).collect();
        let slots = SlotMap {
            primary: c.u16(),
            dynamic: c.u16(),
            field_vocab_id: c.u16(),
            field_vocab_name: c.u16(),
            predicate_vocab_id: c.u16(),
            predicate_vocab_name: c.u16(),
            declared_index_base: c.u16(),
            declared_reduce_base: c.u16(),
            declared_lead_base: c.u16(),
        };
        let n = c.u16() as usize;
        let indexes: Vec<IndexSchema> = (0..n).map(|_| c.index()).collect();
        let faces = c.bytes();
        let n = c.u16() as usize;
        let redacted: Vec<String> = (0..n).map(|_| c.str()).collect();
        assert!(
            c.b.is_empty(),
            "schema: {} trailing bytes this build did not write",
            c.b.len()
        );
        CollectionSchema {
            name,
            key_len,
            key_fields,
            ns_prefix,
            layout_version,
            hot_width,
            hot_fields,
            cold_fields,
            members,
            member_widths,
            slots,
            indexes,
            faces,
            redacted,
        }
    }
}
