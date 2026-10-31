//! A table is a key type plus a row type (ADR-0044): the two are SEPARATE
//! declarations, so one key type can address several tables, and a name may
//! appear in the key and in the payload at the same time — two unrelated fields.
//!
//! Nothing here reconciles the copies, and that is the contract being locked.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::collections::BTreeMap;

use okm_core::obj_dynamic::DynamicValue;
use okm_core::{Document, DocumentEncode, KeyEncode, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct ProfileKey {
    pub org_id: u32,
    pub user_id: u64,
}

/// `user_id` is BOTH a key field and a payload field. They are two fields that
/// happen to share a name, not one field stored twice.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ProfileKey)]
#[ok_ns(41)]
#[ok_name("profile")]
pub struct Profile {
    pub user_id: u64,
    pub bio: String,
}

/// A side table over the SAME key type: byte-identical key payload, own ns.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ProfileKey)]
#[ok_ns(42)]
#[ok_name("profile_stats")]
pub struct ProfileStats {
    pub views: u32,
}

fn key(user_id: u64) -> ProfileKey {
    ProfileKey { org_id: 7, user_id }
}

fn map_of(pairs: &[(&str, DynamicValue)]) -> BTreeMap<String, DynamicValue> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

#[test]
fn a_name_may_live_in_the_key_and_in_the_payload() {
    let store = TestStore::default();
    let table = <Profile as Document>::collection(store.clone());
    // The address says 101; the payload says 999. Both are legal and unrelated.
    table
        .put(
            &key(101),
            &Profile {
                user_id: 999,
                bio: "x".into(),
            },
        )
        .unwrap();

    let row = table.get(&key(101)).unwrap().unwrap();
    assert_eq!(row.user_id, 999, "the payload copy is its own field");
    assert!(
        table.get(&key(999)).unwrap().is_none(),
        "the key's value still addresses"
    );

    let doc = table.fetch(&key(101)).unwrap().unwrap();
    assert_eq!(
        doc.get("user_id"),
        Some(&DynamicValue::UInt(999)),
        "the map carries the payload copy, never the key's"
    );
}

#[test]
fn the_same_key_type_addresses_a_side_table() {
    let store = TestStore::default();
    let main = <Profile as Document>::collection(store.clone());
    let side = <ProfileStats as Document>::collection(store.clone());
    let k = key(42);

    main.put(
        &k,
        &Profile {
            user_id: 42,
            bio: "hello".into(),
        },
    )
    .unwrap();
    side.put(&k, &ProfileStats { views: 3 }).unwrap();

    // Byte-identical key payload: the two primary keys differ only in the ns.
    let a = main.primary_key(&k);
    let b = side.primary_key(&k);
    assert_eq!(a.len(), b.len());
    assert_ne!(a[..2], b[..2], "different tables, different ns");
    assert_eq!(
        a[3..],
        b[3..],
        "the key payload is the same bytes by construction"
    );

    assert_eq!(main.get(&k).unwrap().unwrap().bio, "hello");
    assert_eq!(side.get(&k).unwrap().unwrap().views, 3);
}

#[test]
fn a_map_name_that_matches_a_payload_field_writes_the_row() {
    let store = TestStore::default();
    let table = <Profile as Document>::collection(store.clone());
    let k = key(7);
    table
        .put(
            &k,
            &Profile {
                user_id: 1,
                bio: "a".into(),
            },
        )
        .unwrap();

    // The classifier is the payload declaration, not the key: `user_id` is a
    // declared field, so this is a row update (the segment never sees it).
    table
        .merge(&k, &map_of(&[("user_id", DynamicValue::UInt(50))]))
        .unwrap();

    let row = table.get(&k).unwrap().unwrap();
    assert_eq!(row.user_id, 50);
    assert_eq!(row.bio, "a", "other payload fields are untouched");

    // A name the payload does NOT declare goes to the dynamic segment.
    table
        .merge(&k, &map_of(&[("nickname", DynamicValue::Str("z".into()))]))
        .unwrap();
    let doc = table.fetch(&k).unwrap().unwrap();
    assert_eq!(doc.get("nickname"), Some(&DynamicValue::Str("z".into())));
    assert_eq!(doc.get("user_id"), Some(&DynamicValue::UInt(50)));
}
