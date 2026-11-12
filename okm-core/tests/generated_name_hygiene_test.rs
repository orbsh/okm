//! Generated code is hygienic where it shares a scope with the caller's names
//! (ADR-0053).
//!
//! The derive's decoders bind one local per declared field, in the same scope
//! as the generated cursor/parameter names. Before the rule, a payload field
//! called `b` shadowed the decoder's byte-slice parameter and the generated
//! `let cold_end = b.len()` stopped compiling — a field name could break the
//! macro output. The names below are exactly the generated ones, so this file
//! fails to compile if the property is lost.
//!
//! It also fails if the property is "fixed" the other way, by renaming the
//! field bindings: `Self { b, offset, … }` reads call-site idents.

#![allow(clippy::unwrap_used, clippy::panic)]

use okm_core::{Collection, Document, DocumentEncode, KeyEncode};
use okm_value::DynamicValue;

use okm_async::TestStore;
use okm_core::parts;
/// A key whose fields collide with `KeyEncode::decode`'s own names.
#[derive(KeyEncode, Clone, PartialEq, Eq, Debug)]
pub struct ShadowKey {
    pub org: u32,
    pub b: u64,
    pub offset: u16,
}

/// Every name the payload/covered/segment decoders use, plus one cold field so
/// the cold walk (and its `len` local) is exercised too.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ShadowKey)]
#[ok_ns(250)]
#[ok_index(by_b { fields(b) })]
#[ok_index(by_cov { fields(ver), includes(len) })]
pub struct Shadow {
    pub b: u64,
    pub offset: u64,
    pub ver: u64,
    pub hot_len: u64,
    pub hot_end: u64,
    pub cold_end: u64,
    pub cold_pos: u64,
    pub v: u64,
    pub len: u64,
    pub buf: u64,
    pub out: u64,
    pub map: u64,
    pub name: String, // cold: variable width, must stay last
}

fn row() -> Shadow {
    Shadow {
        b: 11,
        offset: 12,
        ver: 13,
        hot_len: 14,
        hot_end: 15,
        cold_end: 16,
        cold_pos: 17,
        v: 18,
        len: 19,
        buf: 20,
        out: 21,
        map: 22,
        name: "cold".to_string(),
    }
}

fn key() -> ShadowKey {
    ShadowKey {
        org: 7,
        b: 3,
        offset: 4,
    }
}

#[test]
fn payload_and_key_round_trip() {
    let col: Collection<_, ShadowKey, Shadow> = Collection::new(TestStore::default());
    col.put(&key(), &row()).unwrap();
    assert_eq!(col.get(&key()).unwrap(), Some(row()));

    // KeyEncode::decode binds one local per key field in the same scope.
    let encoded = key().encode();
    assert_eq!(ShadowKey::decode(&encoded), key());
}

#[test]
fn the_map_bridge_round_trips() {
    let r = row();
    let map = r.to_map();
    assert_eq!(Shadow::try_from_map(&map).unwrap(), r);
}

#[test]
fn index_scans_reach_the_hostile_named_fields() {
    let col: Collection<_, ShadowKey, Shadow> = Collection::new(TestStore::default());
    col.put(&key(), &row()).unwrap();

    // A typed probe over a field called `b` (the builder method takes that name).
    let probe = __OkmIndex_Shadow_by_b::prefix().b(11u64).finish();
    let hits: Vec<ShadowKey> = col
        .scan::<__OkmIndex_Shadow_by_b, (parts::Fields, parts::Key, parts::Row)>(&probe)
        .collect::<okm_core::error::Result<Vec<_>>>()
        .unwrap()
        .into_iter()
        .map(|(_, pk, _)| pk.decoded)
        .collect();
    assert_eq!(hits, vec![key()]);

    // Covered read: `decode_covered` binds `len` — the field it names.
    let probe = __OkmIndex_Shadow_by_cov::prefix().ver(13u64).finish();
    let covered = col
        .scan::<__OkmIndex_Shadow_by_cov, (parts::Fields, parts::Key, parts::Include)>(
            probe.as_bytes(),
        )
        .collect::<okm_core::error::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(covered.len(), 1);
    assert_eq!(covered[0].2.len, 19);
}

#[test]
fn the_dynamic_segment_still_takes_unknown_names() {
    let col: Collection<_, ShadowKey, Shadow> = Collection::new(TestStore::default());
    col.put(&key(), &row()).unwrap();

    let mut patch = std::collections::BTreeMap::new();
    patch.insert("note".to_string(), DynamicValue::Str("open segment".into()));
    patch.insert("b".to_string(), DynamicValue::U64(99));
    col.merge(&key(), &patch).unwrap();

    let stored = col.get(&key()).unwrap().unwrap();
    assert_eq!(stored.b, 99, "a declared name routes to the row");
    assert_eq!(stored.name, "cold", "the rest of the row is untouched");
}
