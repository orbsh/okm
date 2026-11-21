//! Entry production from a compiled declaration (ADR-0076 §1).
//!
//! Locks the property that makes one producer possible: the bytes a face's
//! entries carry are a function of the declaration and the row's KEY and
//! PAYLOAD bytes alone — no typed accessors, no per-face generated body. The
//! test drives the generated producer and the compiled walk over the same row
//! and requires byte equality, face by face, including a cold `Str` field, a
//! covered segment and a `key(...)`-truncated tail.

#![allow(clippy::unwrap_used, clippy::panic)]

use okm_core::model::entries::Entries;
use okm_core::model::entry_layout::{EntryLayout, FaceDecl};
use okm_core::schema::CollectionSchema;
use okm_core::{Document, DocumentEncode, KeyEncode, KvIndex};

#[derive(KeyEncode, Clone, PartialEq, Eq, Debug)]
pub struct EvKey {
    pub id: u64,
    pub ts: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EvKey)]
#[ok_ns(217)]
#[ok_index(by_level { fields(level) includes(score) })]
#[ok_index(by_level_name { fields(level, name), key(ts) })]
#[ok_lead(by_ts { fields(ts), key(ts) })]
pub struct EvRow {
    pub level: u32,
    pub name: String,
    pub score: u64,
}

/// The generated faces, as the layout compiler reads them: every fact comes off
/// the generated `KvIndex` impl, so the two sides cannot disagree about what the
/// declaration says.
fn decls() -> [FaceDecl<'static>; 3] {
    [
        FaceDecl {
            name: "by_level",
            slot: <__OkmIndex_EvRow_by_level as KvIndex>::SLOT,
            fields: <__OkmIndex_EvRow_by_level as KvIndex>::FIELDS,
            includes: <__OkmIndex_EvRow_by_level as KvIndex>::INCLUDES,
            key_prefix: <__OkmIndex_EvRow_by_level as KvIndex>::KEY_PREFIX,
            lead: false,
            func: false,
            partial: false,
        },
        FaceDecl {
            name: "by_level_name",
            slot: <__OkmIndex_EvRow_by_level_name as KvIndex>::SLOT,
            fields: <__OkmIndex_EvRow_by_level_name as KvIndex>::FIELDS,
            includes: <__OkmIndex_EvRow_by_level_name as KvIndex>::INCLUDES,
            key_prefix: <__OkmIndex_EvRow_by_level_name as KvIndex>::KEY_PREFIX,
            lead: false,
            func: false,
            partial: false,
        },
        FaceDecl {
            name: "by_ts",
            slot: <__OkmLead_EvRow_by_ts as KvIndex>::SLOT,
            fields: <__OkmLead_EvRow_by_ts as KvIndex>::FIELDS,
            includes: <__OkmLead_EvRow_by_ts as KvIndex>::INCLUDES,
            key_prefix: <__OkmLead_EvRow_by_ts as KvIndex>::KEY_PREFIX,
            lead: true,
            func: false,
            partial: false,
        },
    ]
}

fn pairs(entries: &Entries) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = entries
        .iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect();
    out.sort();
    out
}

#[test]
fn a_compiled_declaration_produces_the_bytes_the_derive_does() {
    let key = EvKey { id: 7, ts: 11 };
    let row = EvRow {
        level: 3,
        name: "alpha".to_string(),
        score: 9,
    };

    let mut generated = Entries::new();
    <EvRow as Document>::index_entries_into(&key, &row, EvRow::NS_PREFIX, &mut generated);

    let schema = CollectionSchema::of::<EvKey, EvRow>();
    let layout = EntryLayout::compile(&schema, &decls()).unwrap();
    let mut key_bytes = Vec::new();
    key.encode_into(&mut key_bytes);
    let mut produced = Entries::new();
    layout
        .produce(
            EvRow::NS_PREFIX,
            &key_bytes,
            &row.encode_payload(),
            &mut produced,
        )
        .unwrap();

    assert_eq!(layout.faces(), 3);
    assert_eq!(pairs(&produced), pairs(&generated));
}

#[test]
fn the_walk_refuses_what_the_declaration_bytes_cannot_describe() {
    let schema = CollectionSchema::of::<EvKey, EvRow>();
    let mut face = decls()[0];
    face.func = true;
    let err = EntryLayout::compile(&schema, &[face]).unwrap_err();
    assert_eq!(err.face, "by_level");

    let mut face = decls()[0];
    face.partial = true;
    let err = EntryLayout::compile(&schema, &[face]).unwrap_err();
    assert_eq!(err.face, "by_level");
}

#[test]
fn a_missing_cold_field_is_answered_before_any_entry_is_written() {
    let schema = CollectionSchema::of::<EvKey, EvRow>();
    let layout = EntryLayout::compile(&schema, &decls()).unwrap();
    let key = EvKey { id: 7, ts: 11 };
    let mut key_bytes = Vec::new();
    key.encode_into(&mut key_bytes);

    // A payload carrying only the hot segment: `name` is a cold frame and the
    // second face reads it, so the write is refused rather than half-written.
    let row = EvRow {
        level: 3,
        name: "alpha".to_string(),
        score: 9,
    };
    let payload = row.encode_payload();
    let hot_only = &payload[..schema.hot_width];
    let mut out = Entries::new();
    let err = layout
        .produce(EvRow::NS_PREFIX, &key_bytes, hot_only, &mut out)
        .unwrap_err();
    assert!(matches!(
        err,
        okm_core::model::entry_layout::Malformed::MissingCold { .. }
    ));
    assert_eq!(out.len(), 0);
}
