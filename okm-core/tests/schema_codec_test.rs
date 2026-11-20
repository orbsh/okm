//! The schema's binary form (ADR-0074 §2): the declaration as bytes the store can carry,
//! decoded back to the same value.
//!
//! What is locked here: every `FieldType` and `DefaultValue` variant survives the round trip
//! (a variant added without a tag fails to compile in the encoder's `match`, and an unknown
//! tag panics in the decoder rather than decoding as something else), the derived
//! declarations the rest of the suite writes round-trip unchanged, and bytes this build could
//! not have written are refused instead of guessed at (ADR-0040).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::field::Elem;
use okm_core::model::schema::{
    self, CollectionSchema, DefaultValue, FieldSchema, IndexSchema, SlotMap,
};
use okm_core::{DocumentEncode, FieldType, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct RowKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_name("codec_rows")]
#[ok_ref(RowKey)]
#[ok_ns(0x2A)]
#[ok_index(by_name { fields(name) })]
pub struct Row {
    pub level: u32,
    pub name: String,
}

fn field(name: &str, ty: FieldType, width: usize, offset: usize) -> FieldSchema {
    FieldSchema {
        name: name.to_string(),
        ty,
        width,
        offset,
        tag: None,
        default: None,
        expect_len: None,
    }
}

/// A declaration that uses every tag the encoder has, built by hand: the derived fixtures in
/// the suite are ordinary, and the interesting part of a codec is the corners.
fn everything() -> CollectionSchema {
    CollectionSchema {
        name: "corners".to_string(),
        key_len: 24,
        key_fields: vec![
            field("k_u64", FieldType::U64, 8, 0),
            field("k_uuid7", FieldType::Uuid7, 16, 8),
        ],
        ns_prefix: vec![0x00, 0x2B],
        layout_version: 7,
        hot_width: 61,
        hot_fields: vec![
            field("u8", FieldType::U8, 1, 0),
            field("u16", FieldType::U16, 2, 1),
            field("u32", FieldType::U32, 4, 3),
            field("u64", FieldType::U64, 8, 7),
            field("fixed", FieldType::FixedBytes, 16, 15),
            field("varint", FieldType::VarInt, 0, 31),
            field("quant", FieldType::Quant(6), 8, 31),
            field("offset", FieldType::Offset(-9_000_000), 4, 39),
            field("uuid4", FieldType::Uuid4, 16, 43),
        ],
        cold_fields: vec![
            FieldSchema {
                tag: Some(0),
                expect_len: Some(768),
                ..field("vec", FieldType::Vector(Elem::F32), 0, 0)
            },
            FieldSchema {
                tag: Some(1),
                default: Some(DefaultValue::Str("anon".to_string())),
                ..field("str", FieldType::Str, 0, 0)
            },
            FieldSchema {
                tag: Some(2),
                default: Some(DefaultValue::ZeroBytes(12)),
                ..field("bytes", FieldType::Bytes, 0, 0)
            },
            FieldSchema {
                tag: Some(3),
                default: Some(DefaultValue::U64(u64::MAX)),
                ..field("vocab", FieldType::Vocab(0x0123), 1, 0)
            },
            FieldSchema {
                tag: Some(4),
                default: Some(DefaultValue::I64(-3)),
                ..field("vector_str", FieldType::Vector(Elem::Str), 0, 0)
            },
            FieldSchema {
                tag: Some(5),
                default: Some(DefaultValue::F64(-0.0)),
                ..field("vector_i64", FieldType::Vector(Elem::I64), 0, 0)
            },
            FieldSchema {
                tag: Some(6),
                default: Some(DefaultValue::Bool(true)),
                ..field("vector_u32", FieldType::Vector(Elem::U32), 0, 0)
            },
            FieldSchema {
                tag: Some(7),
                ..field("vector_i32", FieldType::Vector(Elem::I32), 0, 0)
            },
            FieldSchema {
                tag: Some(8),
                ..field("vector_u64", FieldType::Vector(Elem::U64), 0, 0)
            },
        ],
        members: vec![0x0031, 0x0032],
        member_widths: vec![(0x0031, 16), (0x0032, 32)],
        slots: SlotMap::standard(),
        indexes: vec![
            IndexSchema {
                name: "by_name".to_string(),
                slot: 0x6A00,
                fields: vec!["name".to_string(), "level".to_string()],
                includes: vec!["name".to_string()],
                func: "lower".to_string(),
                deprecated: false,
            },
            IndexSchema {
                name: "old".to_string(),
                slot: 0x6A01,
                fields: vec![],
                includes: vec![],
                func: String::new(),
                deprecated: true,
            },
        ],
        faces: vec![1, 2, 3, 4, 5, 6, 7, 8],
        redacted: vec!["str".to_string(), "bytes".to_string()],
    }
}

#[test]
fn a_derived_declaration_round_trips() {
    let derived: CollectionSchema = CollectionSchema::of::<RowKey, Row>();
    let bytes = derived.encode();
    let back = CollectionSchema::decode(&bytes);
    assert_eq!(
        back, derived,
        "the derived declaration changed in the round trip"
    );
    assert!(!bytes.is_empty());
    assert!(
        bytes.len() < 2048,
        "a declaration has to stay small enough to sit in the meta row: {} bytes",
        bytes.len()
    );
}

#[test]
fn every_corner_of_the_declaration_round_trips() {
    let all = everything();
    let bytes = all.encode();
    assert_eq!(CollectionSchema::decode(&bytes), all);
    // Re-encoding the decoded value must reproduce the same bytes: a codec that is only
    // "equal after decode" can still write two byte strings for one value.
    assert_eq!(CollectionSchema::decode(&bytes).encode(), bytes);
}

#[test]
fn an_empty_declaration_is_still_a_declaration() {
    // A layout-only declaration (no ns, no name) is legal and must not be mistaken for an
    // error: the round trip is the only thing being asked here.
    let empty = CollectionSchema {
        name: String::new(),
        key_len: 8,
        key_fields: vec![field("id", FieldType::U64, 8, 0)],
        ns_prefix: vec![],
        layout_version: 0,
        hot_width: 0,
        hot_fields: vec![],
        cold_fields: vec![],
        members: vec![],
        member_widths: vec![],
        slots: SlotMap::standard(),
        indexes: vec![],
        faces: vec![],
        redacted: vec![],
    };
    assert_eq!(CollectionSchema::decode(&empty.encode()), empty);
}

#[test]
#[should_panic(expected = "schema: format")]
fn a_format_this_build_does_not_write_is_refused() {
    let mut bytes = everything().encode();
    bytes[0] = schema::SCHEMA_FORMAT + 1;
    let _ = CollectionSchema::decode(&bytes);
}

#[test]
#[should_panic(expected = "trailing bytes")]
fn trailing_bytes_are_refused() {
    let mut bytes = everything().encode();
    bytes.push(0);
    let _ = CollectionSchema::decode(&bytes);
}

#[test]
#[should_panic(expected = "field type tag")]
fn a_type_tag_this_build_does_not_know_is_refused() {
    // fmt, layout, name "", ns "", key_len 0, one key field, its name "", then the tag.
    let bytes = vec![schema::SCHEMA_FORMAT, 0, 0, 0, 0, 0, 1, 0, 250];
    let _ = CollectionSchema::decode(&bytes);
}
