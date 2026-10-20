//! ADR-0012: obj dynamic-segment read/write API over slots 0/1 and the
//! field-name vocabulary (ADR-0032: counting family 4, counters 2/3).

use okm_core::{
    KeyEncode, DocumentEncode, Collection, TestStore, VirtualStorage,
};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(9)]
pub struct User {
    pub legacy: u32,
    pub level: u32,
}

// ---- ADR-0012: obj dynamic-segment API over the two slots ----

#[test]
fn obj_variants_round_trip_over_two_slots() {
    let mut t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&UserKey { id: 1 }, &User { legacy: 1, level: 2 });

    // No dynamic entry yet.
    assert!(t.get_fields(&UserKey { id: 1 }).is_none());

    let mut v = std::collections::BTreeMap::new();
    v.insert("score".to_string(), okm_core::obj_dynamic::DynamicValue::UInt(99));
    v.insert("note".to_string(), okm_core::obj_dynamic::DynamicValue::Str("hello".into()));
    v.insert("flag".to_string(), okm_core::obj_dynamic::DynamicValue::Bool(true));
    t.put_fields(&UserKey { id: 1 }, &v);

    // Name-keyed read; ids live only inside the frames.
    let got = t.get_fields(&UserKey { id: 1 }).unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got["score"], okm_core::obj_dynamic::DynamicValue::UInt(99));
    assert_eq!(got["note"], okm_core::obj_dynamic::DynamicValue::Str("hello".into()));
    assert_eq!(got["flag"], okm_core::obj_dynamic::DynamicValue::Bool(true));

    // Typed get is untouched: declared fields live in slot 0.
    assert_eq!(t.get(&UserKey { id: 1 }).unwrap().level, 2);

    // Vocabulary landed in the vocabulary family (ADR-0032: family 4, counter 2)
    // under the same ns.
    let mut vocab_prefix = vec![0u8, 9];
    okm_core::write_slot(&mut vocab_prefix, okm_core::index::FIELD_VOCAB_ID_SLOT);
    let vocab_scan = t.store().scan_suffix(&vocab_prefix);
    assert_eq!(vocab_scan.len(), 3, "three names allocated");
}

#[test]
fn obj_set_variants_replaces_wholesale_and_delete_clears() {
    let mut t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&UserKey { id: 1 }, &User { legacy: 1, level: 2 });

    let mut v1 = std::collections::BTreeMap::new();
    v1.insert("a".to_string(), okm_core::obj_dynamic::DynamicValue::UInt(1));
    v1.insert("b".to_string(), okm_core::obj_dynamic::DynamicValue::UInt(2));
    t.put_fields(&UserKey { id: 1 }, &v1);

    // Wholesale replace: `a` disappears (it is not merged).
    let mut v2 = std::collections::BTreeMap::new();
    v2.insert("c".to_string(), okm_core::obj_dynamic::DynamicValue::Str("x".into()));
    t.put_fields(&UserKey { id: 1 }, &v2);
    let got = t.get_fields(&UserKey { id: 1 }).unwrap();
    assert_eq!(got.len(), 1);
    assert!(got.contains_key("c"));

    assert!(t.delete_fields(&UserKey { id: 1 }));
    assert!(t.get_fields(&UserKey { id: 1 }).is_none());
    assert!(!t.delete_fields(&UserKey { id: 1 }), "second delete is a no-op");
}

#[test]
fn obj_bridge_typed_and_dynamic_merge() {
    let mut t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&UserKey { id: 1 }, &User { legacy: 5, level: 7 });

    // put_document: declared fields route to slot 0, unknown to slot 1.
    let mut obj = std::collections::BTreeMap::new();
    obj.insert("level".to_string(), okm_core::obj_dynamic::DynamicValue::UInt(30));
    obj.insert("extra".to_string(), okm_core::obj_dynamic::DynamicValue::Str("dyn".into()));
    t.put_document(&UserKey { id: 1 }, &obj);

    // Typed path wrote slot 0 (level 5 → 30); declared field `legacy`
    // kept its value (RMW, not default-clobbered).
    let document = t.get(&UserKey { id: 1 }).unwrap();
    assert_eq!(document.level, 30);
    assert_eq!(document.legacy, 5);

    // get_document merges: declared (to_map) + dynamic (vocabulary).
    let got = t.get_document(&UserKey { id: 1 }).unwrap();
    assert_eq!(got["level"], okm_core::obj_dynamic::DynamicValue::UInt(30));
    assert_eq!(got["legacy"], okm_core::obj_dynamic::DynamicValue::UInt(5));
    assert_eq!(got["extra"], okm_core::obj_dynamic::DynamicValue::Str("dyn".into()));

    // Fresh document via put_document: absent declared fields default, no get first.
    let mut obj2 = std::collections::BTreeMap::new();
    obj2.insert("id".to_string(), okm_core::obj_dynamic::DynamicValue::UInt(2));
    obj2.insert("level".to_string(), okm_core::obj_dynamic::DynamicValue::UInt(1));
    obj2.insert("legacy".to_string(), okm_core::obj_dynamic::DynamicValue::UInt(3));
    t.put_document(&UserKey { id: 2 }, &obj2);
    let row2 = t.get(&UserKey { id: 2 }).unwrap();
    assert_eq!((row2.legacy, row2.level), (3, 1));

    // Unknown names allocate into the vocabulary (external-data rule),
    // and get_document surfaces them.
    assert!(!t.get_fields(&UserKey { id: 2 }).unwrap().is_empty());
}
