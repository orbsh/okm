//! The sidecar is the half of a declaration the log cannot carry (ADR-0056 §7), so
//! it has to survive a round trip for EVERY field kind — not just the ones a test
//! happened to use. The hand-written `Deserialize for FieldType` used to omit
//! `Bytes`, `Vocab`, `Uuid4` and `Uuid7`: a table with a categorical field wrote a
//! sidecar nothing could read back, which is exactly the one promise the sidecar
//! makes. This file locks the whole kind set.

#![cfg(feature = "schema-serde")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::field::{Elem, FieldType};
use okm_core::schema::FieldSchema;

const ALL: &[FieldType] = &[
    FieldType::U8,
    FieldType::U16,
    FieldType::U32,
    FieldType::U64,
    FieldType::FixedBytes,
    FieldType::Str,
    FieldType::Bytes,
    FieldType::VarInt,
    FieldType::Quant(3),
    FieldType::Vocab(0x11),
    FieldType::Offset(-7),
    FieldType::Uuid4,
    FieldType::Uuid7,
    FieldType::Vector(Elem::F32),
    FieldType::Vector(Elem::Str),
];

#[test]
fn every_field_kind_survives_the_sidecar() {
    for ty in ALL {
        let field = FieldSchema {
            name: "f".into(),
            ty: *ty,
            offset: 0,
            width: 8,
            tag: Some(1),
            default: None,
            expect_len: None,
        };
        let json = serde_json::to_string(&field).expect("serializes");
        let back: FieldSchema = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("{ty:?} does not read back: {e} — {json}"));
        assert_eq!(&back.ty, ty, "round trip changed the kind: {json}");
    }
}

/// Sidecars written before `Elem` existed carry the Rust type name
/// (`{"Vector":"f32"}`); the enum's aliases keep them readable, because a written
/// log is not something a later refactor gets to invalidate.
#[test]
fn a_sidecar_from_before_the_element_enum_still_reads() {
    let old = r#"{"name":"v","ty":{"Vector":"f32"},"width":0,"offset":0,"tag":null,"default":null,"expect_len":null}"#;
    let f: FieldSchema = serde_json::from_str(old).expect("an older sidecar reads");
    assert_eq!(f.ty, FieldType::Vector(Elem::F32));

    let old = r#"{"name":"v","ty":{"Vector":"String"},"width":0,"offset":0,"tag":null,"default":null,"expect_len":null}"#;
    let f: FieldSchema = serde_json::from_str(old).expect("an older sidecar reads");
    assert_eq!(f.ty, FieldType::Vector(Elem::Str));
}
