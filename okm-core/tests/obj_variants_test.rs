//! ADR-0035: the whole-document API over slots 0 and 1 (ADR-0012 layout,
//! ADR-0032 vocabulary): one name-keyed map surface — `view` / `merge` /
//! `replace` / `purge` — beside the typed `get` / `put` / `delete`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

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
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(1, 2)).unwrap();

    // A static-only document: the view carries the declared names.
    let row_only = t.fetch(&key(1)).unwrap().unwrap();
    assert_eq!(row_only.len(), 2);
    assert_eq!(row_only["level"], DynamicValue::UInt(2));

    // Dynamic names join the same view; the typed read stays slot 0.
    t.merge(
        &key(1),
        &dmap(&[
            ("score", DynamicValue::UInt(99)),
            ("note", DynamicValue::Str("hello".into())),
        ]),
    )
    .unwrap();
    let got = t.fetch(&key(1)).unwrap().unwrap();
    assert_eq!(got.len(), 4);
    assert_eq!(got["score"], DynamicValue::UInt(99));
    assert_eq!(got["note"], DynamicValue::Str("hello".into()));
    assert_eq!(t.get(&key(1)).unwrap().unwrap(), user(1, 2));

    // Vocabulary landed in the id family under the same ns, cursor included
    // (ADR-0032: the face holds its allocation cursor at the bare prefix).
    let mut prefix = vec![0u8, 9];
    okm_core::write_slot(&mut prefix, okm_core::index::FIELD_VOCAB_ID_SLOT);
    let scan = t.store().scan_suffix(&prefix).unwrap();
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
    t.merge(&key(9), &dmap(&[("only", DynamicValue::UInt(1))]))
        .unwrap();
    assert!(t.get(&key(9)).unwrap().is_none(), "no static segment");
    assert_eq!(t.fetch(&key(9)).unwrap().unwrap().len(), 1);
}

#[test]
fn merge_touches_only_the_names_it_is_given() {
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(1, 5)).unwrap();

    t.merge(
        &key(1),
        &dmap(&[("a", DynamicValue::UInt(1)), ("b", DynamicValue::UInt(2))]),
    )
    .unwrap();
    // Per-name: `c` joins, `a` and `b` survive (a wholesale write would drop them).
    t.merge(&key(1), &dmap(&[("c", DynamicValue::UInt(3))]))
        .unwrap();

    let got = t.fetch(&key(1)).unwrap().unwrap();
    assert_eq!(got.len(), 5, "2 declared + 3 dynamic");
    for name in ["a", "b", "c"] {
        assert!(
            got.contains_key(name),
            "{name} must survive a partial merge"
        );
    }
}

#[test]
fn merge_of_a_declared_field_is_an_rmw_and_cleans_the_old_face() {
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(1, 5)).unwrap();
    assert_eq!(t.scan::<ByLevel>(&5u32.to_be_bytes()).unwrap().len(), 1);

    t.merge(&key(1), &dmap(&[("level", DynamicValue::UInt(30))]))
        .unwrap();

    let row = t.get(&key(1)).unwrap().unwrap();
    assert_eq!(
        (row.legacy, row.level),
        (1, 30),
        "unnamed declared field kept"
    );
    assert!(
        t.scan::<ByLevel>(&5u32.to_be_bytes()).unwrap().is_empty(),
        "the previous value's face is gone — merge cleans, put does not"
    );
    assert_eq!(t.scan::<ByLevel>(&30u32.to_be_bytes()).unwrap().len(), 1);
}

#[test]
fn replace_is_the_whole_document() {
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(9, 9)).unwrap();
    t.merge(&key(1), &dmap(&[("gone", DynamicValue::UInt(1))]))
        .unwrap();

    // `legacy` is not named → default; the dynamic segment is EXACTLY the map's
    // non-declared names, so `gone` disappears.
    t.replace(
        &key(1),
        &dmap(&[
            ("level", DynamicValue::UInt(4)),
            ("kept", DynamicValue::Str("y".into())),
        ]),
    )
    .unwrap();
    assert_eq!(t.get(&key(1)).unwrap().unwrap(), user(0, 4));
    let got = t.fetch(&key(1)).unwrap().unwrap();
    assert!(
        !got.contains_key("gone"),
        "replace drops unnamed dynamic names"
    );
    assert_eq!(got["kept"], DynamicValue::Str("y".into()));
    assert!(
        t.scan::<ByLevel>(&9u32.to_be_bytes()).unwrap().is_empty(),
        "the old face is gone"
    );

    // A fresh key: the map alone builds the row (absent fields default).
    t.replace(&key(3), &dmap(&[("legacy", DynamicValue::UInt(7))]))
        .unwrap();
    assert_eq!(t.get(&key(3)).unwrap().unwrap(), user(7, 0));

    // An empty map IS the empty document.
    t.replace(&key(1), &BTreeMap::new()).unwrap();
    assert!(t.fetch(&key(1)).unwrap().is_none());
}

#[test]
fn purge_clears_both_slots_in_one_call() {
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(2), &user(1, 1)).unwrap();
    t.merge(&key(2), &dmap(&[("x", DynamicValue::UInt(1))]))
        .unwrap();
    assert!(t.fetch(&key(2)).unwrap().is_some());

    assert!(t.purge(&key(2)).unwrap());
    assert!(t.fetch(&key(2)).unwrap().is_none(), "both slots gone");
    assert!(!t.purge(&key(2)).unwrap(), "second purge is a no-op");

    // A dynamic-only document is purgeable too.
    t.merge(&key(4), &dmap(&[("y", DynamicValue::UInt(1))]))
        .unwrap();
    assert!(t.purge(&key(4)).unwrap());
    assert!(t.fetch(&key(4)).unwrap().is_none());
}

// ---- from_map 的两条规则：缺席填默认，出现但种类/范围不对则具名拒绝 ----------

#[test]
fn from_map_defaults_an_absent_name() {
    use okm_core::Document;
    // 缺失的名字是版本演进的正常形态（旧 map 本来就没有这个字段）：取声明默认。
    let doc = User::try_from_map(&dmap(&[("level", DynamicValue::UInt(7))])).unwrap();
    assert_eq!(
        doc,
        User {
            legacy: 0,
            level: 7
        }
    );
}

#[test]
fn from_map_refuses_a_present_name_of_the_wrong_kind() {
    use okm_core::Document;
    // 出现但种类不对是调用方错误：静默填默认等于写入调用方从没要过的值。
    let err =
        User::try_from_map(&dmap(&[("level", DynamicValue::Str("seven".into()))])).unwrap_err();
    assert_eq!(
        err.to_string(),
        "from_map: field `level` declares UInt; the map carries Str"
    );
}

#[test]
fn from_map_refuses_a_value_that_does_not_fit_the_declared_width() {
    use okm_core::Document;
    // 种类对、宽度不对：截断和填默认一样是凭空造数据。
    let err = User::try_from_map(&dmap(&[("level", DynamicValue::UInt(u32::MAX as u64 + 1))]))
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "from_map: field `level` declares u32; the map carries 4294967296, which cannot be represented"
    );
}

#[test]
fn a_refused_merge_returns_the_error_and_writes_nothing() {
    // 拒绝发生在任何 op 进 batch 之前：存储原封不动，调用方拿到 Err 而不是
    // 一个静默改过的文档（也不是 panic —— 这是调用方数据，不是故障）。
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(3, 4)).unwrap();

    let err = t
        .merge(&key(1), &dmap(&[("level", DynamicValue::Str("x".into()))]))
        .unwrap_err();
    assert!(
        matches!(err, okm_core::Error::KindMismatch { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        t.get(&key(1)).unwrap().unwrap(),
        user(3, 4),
        "a refused merge writes nothing"
    );

    // 同一个 API 上的成功路径不受影响。
    t.merge(&key(1), &dmap(&[("level", DynamicValue::UInt(9))]))
        .unwrap();
    assert_eq!(t.get(&key(1)).unwrap().unwrap(), user(3, 9));
}
