//! ADR-0035: the whole-document API over slots 0 and 1 (ADR-0012 layout,
//! ADR-0032 vocabulary): one name-keyed map surface — `view` / `merge` /
//! `replace` / `purge` — beside the typed `get` / `put` / `delete`.

use std::collections::BTreeMap;

use okm_core::obj_dynamic::DynamicValue;
use okm_core::{Collection, DocumentEncode, KeyEncode, TestStore, VirtualStorage};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_index(by_level { fields(level) })]
#[ok_ns(9)]
pub struct User {
    pub legacy: u32,
    pub level: u32,
}

use __OkmIndex_User_by_level as ByLevel;

fn key(id: u64) -> UserKey {
    UserKey { id }
}

fn user(legacy: u32, level: u32) -> User {
    User { legacy, level }
}

fn dmap(pairs: &[(&str, DynamicValue)]) -> BTreeMap<String, DynamicValue> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

#[test]
fn view_projects_both_slots_and_leaves_the_typed_read_alone() {
    let mut t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(1, 2));

    // A static-only document: the view carries the declared names.
    let row_only = t.fetch(&key(1)).unwrap();
    assert_eq!(row_only.len(), 2);
    assert_eq!(row_only["level"], DynamicValue::UInt(2));

    // Dynamic names join the same view; the typed read stays slot 0.
    t.merge(
        &key(1),
        &dmap(&[
            ("score", DynamicValue::UInt(99)),
            ("note", DynamicValue::Str("hello".into())),
        ]),
    );
    let got = t.fetch(&key(1)).unwrap();
    assert_eq!(got.len(), 4);
    assert_eq!(got["score"], DynamicValue::UInt(99));
    assert_eq!(got["note"], DynamicValue::Str("hello".into()));
    assert_eq!(t.get(&key(1)).unwrap(), user(1, 2));

    // Vocabulary landed in the id family under the same ns, cursor included
    // (ADR-0032: the face holds its allocation cursor at the bare prefix).
    let mut prefix = vec![0u8, 9];
    okm_core::write_slot(&mut prefix, okm_core::index::FIELD_VOCAB_ID_SLOT);
    let scan = t.store().scan_suffix(&prefix);
    assert_eq!(
        scan.iter().filter(|s| s.len() == 2).count(),
        2,
        "two names allocated"
    );
    assert!(
        scan.iter().any(|s| s.is_empty()),
        "plus the cursor singleton at the bare prefix"
    );

    // A dynamic-only document still views: the document is the PAIR of slots,
    // not the primary entry.
    t.merge(&key(9), &dmap(&[("only", DynamicValue::UInt(1))]));
    assert!(t.get(&key(9)).is_none(), "no static segment");
    assert_eq!(t.fetch(&key(9)).unwrap().len(), 1);
}

#[test]
fn merge_touches_only_the_names_it_is_given() {
    let mut t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(1, 5));

    t.merge(
        &key(1),
        &dmap(&[("a", DynamicValue::UInt(1)), ("b", DynamicValue::UInt(2))]),
    );
    // Per-name: `c` joins, `a` and `b` survive (a wholesale write would drop them).
    t.merge(&key(1), &dmap(&[("c", DynamicValue::UInt(3))]));

    let got = t.fetch(&key(1)).unwrap();
    assert_eq!(got.len(), 5, "2 declared + 3 dynamic");
    for name in ["a", "b", "c"] {
        assert!(got.contains_key(name), "{name} must survive a partial merge");
    }
}

#[test]
fn merge_of_a_declared_field_is_an_rmw_and_cleans_the_old_face() {
    let mut t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(1, 5));
    assert_eq!(t.scan::<ByLevel>(&5u32.to_be_bytes()).len(), 1);

    t.merge(&key(1), &dmap(&[("level", DynamicValue::UInt(30))]));

    let row = t.get(&key(1)).unwrap();
    assert_eq!((row.legacy, row.level), (1, 30), "unnamed declared field kept");
    assert!(
        t.scan::<ByLevel>(&5u32.to_be_bytes()).is_empty(),
        "the previous value's face is gone — merge cleans, put does not"
    );
    assert_eq!(t.scan::<ByLevel>(&30u32.to_be_bytes()).len(), 1);
}

#[test]
fn replace_is_the_whole_document() {
    let mut t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(9, 9));
    t.merge(&key(1), &dmap(&[("gone", DynamicValue::UInt(1))]));

    // `legacy` is not named → default; the dynamic segment is EXACTLY the map's
    // non-declared names, so `gone` disappears.
    t.replace(
        &key(1),
        &dmap(&[
            ("level", DynamicValue::UInt(4)),
            ("kept", DynamicValue::Str("y".into())),
        ]),
    );
    assert_eq!(t.get(&key(1)).unwrap(), user(0, 4));
    let got = t.fetch(&key(1)).unwrap();
    assert!(!got.contains_key("gone"), "replace drops unnamed dynamic names");
    assert_eq!(got["kept"], DynamicValue::Str("y".into()));
    assert!(
        t.scan::<ByLevel>(&9u32.to_be_bytes()).is_empty(),
        "the old face is gone"
    );

    // A fresh key: the map alone builds the row (absent fields default).
    t.replace(&key(3), &dmap(&[("legacy", DynamicValue::UInt(7))]));
    assert_eq!(t.get(&key(3)).unwrap(), user(7, 0));

    // An empty map IS the empty document.
    t.replace(&key(1), &BTreeMap::new());
    assert!(t.fetch(&key(1)).is_none());
}

#[test]
fn purge_clears_both_slots_in_one_call() {
    let mut t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(2), &user(1, 1));
    t.merge(&key(2), &dmap(&[("x", DynamicValue::UInt(1))]));
    assert!(t.fetch(&key(2)).is_some());

    assert!(t.purge(&key(2)));
    assert!(t.fetch(&key(2)).is_none(), "both slots gone");
    assert!(!t.purge(&key(2)), "second purge is a no-op");

    // A dynamic-only document is purgeable too.
    t.merge(&key(4), &dmap(&[("y", DynamicValue::UInt(1))]));
    assert!(t.purge(&key(4)));
    assert!(t.fetch(&key(4)).is_none());
}