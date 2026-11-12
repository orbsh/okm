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

use okm_value::{Dict, Variant};

use okm_async::TestStore;
use okm_core::{Collection, DocumentEncode, KeyEncode, VirtualStorage};

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

/// The document as one map: the row's value with the dynamic segment laid over
/// it — a point read answers parts, a row converts to a value, and values
/// merge. Test-local on purpose: the crate offers no assembly function.
fn doc_of<S, K, R>(
    col: &okm_core::Collection<S, K, R>,
    key: &K,
) -> okm_core::error::Result<Option<Dict>>
where
    S: okm_core::VirtualStorage,
    K: okm_core::KeyEncode,
    R: okm_core::Document<Key = K>,
{
    let (row, seg) = col.get::<(okm_core::parts::Row, okm_core::parts::Value)>(key)?;
    let doc = match (row, seg) {
        (None, None) => return Ok(None),
        (row, seg) => row
            .map(|r| r.to_value())
            .unwrap_or_default()
            .merged(seg.unwrap_or_default()),
    };
    match doc {
        okm_value::Variant::Map(map) => Ok(Some(map)),
        other => Err(okm_core::error::Error::Corrupt {
            what: format!(
                "a document composes to an object, got {}",
                okm_value::dyn_kind(&other)
            ),
        }),
    }
}

fn key(id: u64) -> UserKey {
    UserKey { id }
}

fn user(legacy: u32, level: u32) -> User {
    User { legacy, level }
}

fn dmap(pairs: &[(&str, Variant)]) -> Dict {
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
    let row_only = doc_of(&t, &key(1)).unwrap().unwrap();
    assert_eq!(row_only.len(), 2);
    assert_eq!(row_only["level"], Variant::U64(2));

    // Dynamic names join the same view; the typed read stays slot 0.
    t.put(
        &key(1),
        &Variant::Map(dmap(&[
            ("score", Variant::U64(99)),
            ("note", Variant::Str("hello".into())),
        ])),
    )
    .unwrap();
    let got = doc_of(&t, &key(1)).unwrap().unwrap();
    assert_eq!(got.len(), 4);
    assert_eq!(got["score"], Variant::U64(99));
    assert_eq!(got["note"], Variant::Str("hello".into()));
    assert_eq!(t.get_row(&key(1)).unwrap().unwrap(), user(1, 2));

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
    t.put(&key(9), &Variant::Map(dmap(&[("only", Variant::U64(1))])))
        .unwrap();
    assert!(t.get_row(&key(9)).unwrap().is_none(), "no static segment");
    assert_eq!(doc_of(&t, &key(9)).unwrap().unwrap().len(), 1);
}

#[test]
fn a_point_read_answers_parts_apart() {
    use okm_core::parts;
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(1, 2)).unwrap();
    t.put(
        &key(1),
        &Variant::Map(dmap(&[("note", Variant::Str("hi".into()))])),
    )
    .unwrap();

    // Both halves, kept apart: each is its own Option.
    let (row, value) = t.get::<(parts::Row, parts::Value)>(&key(1)).unwrap();
    assert_eq!(row.unwrap(), user(1, 2));
    match value.unwrap() {
        Variant::Map(map) => {
            assert_eq!(map.len(), 1);
            assert_eq!(map["note"], Variant::Str("hi".into()));
        }
        other => panic!("slot 1 is an object, got {other:?}"),
    }

    // get_row is the sugar for the one-element tuple, so the collapse rule
    // makes the two answers the same type AND the same value.
    assert_eq!(
        t.get::<(parts::Row,)>(&key(1)).unwrap(),
        t.get_row(&key(1)).unwrap()
    );

    // The dynamic half alone, without decoding the row.
    let only = t.get::<(parts::Value,)>(&key(1)).unwrap();
    assert!(only.is_some());

    // Absence is per slot: a dynamic-only document answers (None, Some).
    t.put(&key(9), &Variant::Map(dmap(&[("only", Variant::U64(1))])))
        .unwrap();
    let (row, value) = t.get::<(parts::Row, parts::Value)>(&key(9)).unwrap();
    assert!(row.is_none(), "no row in slot 0");
    assert!(value.is_some(), "slot 1 exists");

    // ...and a key holding nothing answers (None, None), not one None for the
    // pair.
    let (row, value) = t.get::<(parts::Row, parts::Value)>(&key(404)).unwrap();
    assert!(row.is_none());
    assert!(value.is_none());
    assert!(doc_of(&t, &key(404)).unwrap().is_none());
}

#[test]
fn merge_touches_only_the_names_it_is_given() {
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(1, 5)).unwrap();

    t.put(
        &key(1),
        &Variant::Map(dmap(&[("a", Variant::U64(1)), ("b", Variant::U64(2))])),
    )
    .unwrap();
    // Per-name: `c` joins, `a` and `b` survive (a wholesale write would drop them).
    t.put(&key(1), &Variant::Map(dmap(&[("c", Variant::U64(3))])))
        .unwrap();

    let got = doc_of(&t, &key(1)).unwrap().unwrap();
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
    assert_eq!(
        t.scan::<ByLevel>(&5u32.to_be_bytes())
            .collect::<okm_core::error::Result<Vec<_>>>()
            .unwrap()
            .len(),
        1
    );

    t.put(&key(1), &Variant::Map(dmap(&[("level", Variant::U64(30))])))
        .unwrap();

    let row = t.get_row(&key(1)).unwrap().unwrap();
    assert_eq!(
        (row.legacy, row.level),
        (1, 30),
        "unnamed declared field kept"
    );
    assert!(
        t.scan::<ByLevel>(&5u32.to_be_bytes())
            .collect::<okm_core::error::Result<Vec<_>>>()
            .unwrap()
            .is_empty(),
        "the previous value's face is gone — merge cleans, put does not"
    );
    assert_eq!(
        t.scan::<ByLevel>(&30u32.to_be_bytes())
            .collect::<okm_core::error::Result<Vec<_>>>()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn replace_is_the_doc_of() {
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(1), &user(9, 9)).unwrap();
    t.put(&key(1), &Variant::Map(dmap(&[("gone", Variant::U64(1))])))
        .unwrap();

    // `legacy` is not named → default; the dynamic segment is EXACTLY the map's
    // non-declared names, so `gone` disappears.
    t.put(
        &key(1),
        (&Variant::Map(dmap(&[
            ("level", Variant::U64(4)),
            ("kept", Variant::Str("y".into())),
        ])),),
    )
    .unwrap();
    assert_eq!(t.get_row(&key(1)).unwrap().unwrap(), user(0, 4));
    let got = doc_of(&t, &key(1)).unwrap().unwrap();
    assert!(
        !got.contains_key("gone"),
        "replace drops unnamed dynamic names"
    );
    assert_eq!(got["kept"], Variant::Str("y".into()));
    assert!(
        t.scan::<ByLevel>(&9u32.to_be_bytes())
            .collect::<okm_core::error::Result<Vec<_>>>()
            .unwrap()
            .is_empty(),
        "the old face is gone"
    );

    // A fresh key: the map alone builds the row (absent fields default).
    t.put(
        &key(3),
        (&Variant::Map(dmap(&[("legacy", Variant::U64(7))])),),
    )
    .unwrap();
    assert_eq!(t.get_row(&key(3)).unwrap().unwrap(), user(7, 0));

    // An empty map IS the empty document.
    t.put(&key(1), (&Variant::Map(Dict::new()),)).unwrap();
    assert!(doc_of(&t, &key(1)).unwrap().is_none());
}

#[test]
fn purge_clears_both_slots_in_one_call() {
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    t.put(&key(2), &user(1, 1)).unwrap();
    t.put(&key(2), &Variant::Map(dmap(&[("x", Variant::U64(1))])))
        .unwrap();
    assert!(doc_of(&t, &key(2)).unwrap().is_some());

    assert!(t.purge(&key(2)).unwrap());
    assert!(doc_of(&t, &key(2)).unwrap().is_none(), "both slots gone");
    assert!(!t.purge(&key(2)).unwrap(), "second purge is a no-op");

    // A dynamic-only document is purgeable too.
    t.put(&key(4), &Variant::Map(dmap(&[("y", Variant::U64(1))])))
        .unwrap();
    assert!(t.purge(&key(4)).unwrap());
    assert!(doc_of(&t, &key(4)).unwrap().is_none());
}

// ---- from_map 的两条规则：缺席填默认，出现但种类/范围不对则具名拒绝 ----------

#[test]
fn from_map_defaults_an_absent_name() {
    use okm_core::Document;
    // 缺失的名字是版本演进的正常形态（旧 map 本来就没有这个字段）：取声明默认。
    let doc = User::try_from_map(&dmap(&[("level", Variant::U64(7))])).unwrap();
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
    let err = User::try_from_map(&dmap(&[("level", Variant::Str("seven".into()))])).unwrap_err();
    assert_eq!(
        err.to_string(),
        "from_map: field `level` declares UInt; the map carries Str"
    );
}

#[test]
fn from_map_refuses_a_value_that_does_not_fit_the_declared_width() {
    use okm_core::Document;
    // 种类对、宽度不对：截断和填默认一样是凭空造数据。
    let err =
        User::try_from_map(&dmap(&[("level", Variant::U64(u32::MAX as u64 + 1))])).unwrap_err();
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
        .put(
            &key(1),
            &Variant::Map(dmap(&[("level", Variant::Str("x".into()))])),
        )
        .unwrap_err();
    assert!(
        matches!(err, okm_core::Error::KindMismatch { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        t.get_row(&key(1)).unwrap().unwrap(),
        user(3, 4),
        "a refused merge writes nothing"
    );

    // 同一个 API 上的成功路径不受影响。
    t.put(&key(1), &Variant::Map(dmap(&[("level", Variant::U64(9))])))
        .unwrap();
    assert_eq!(t.get_row(&key(1)).unwrap().unwrap(), user(3, 9));
}

/// Two handles on one engine allocate from the SAME cursor: the mirror must not
/// hand out an id another handle just took, so the watermark is read from the
/// store per allocation rather than cached on the mirror.
#[test]
fn two_handles_on_one_engine_do_not_share_an_id() {
    let store = TestStore::slatedb_mem();
    let a: Collection<TestStore, UserKey, User> = Collection::new(store.shared_handle());
    let b: Collection<TestStore, UserKey, User> = Collection::new(store.shared_handle());
    a.put(
        &key(1),
        &Variant::Map(dmap(&[("alpha", Variant::Str("x".into()))])),
    )
    .unwrap();
    b.put(
        &key(2),
        &Variant::Map(dmap(&[("beta", Variant::Str("y".into()))])),
    )
    .unwrap();
    // `a` allocates again AFTER `b` took the next id: a cached watermark would
    // reuse it and repoint `beta`'s label.
    a.put(
        &key(3),
        &Variant::Map(dmap(&[("gamma", Variant::Str("z".into()))])),
    )
    .unwrap();

    assert_eq!(
        doc_of(&a, &key(2)).unwrap().unwrap()["beta"],
        Variant::Str("y".into())
    );
    assert_eq!(
        doc_of(&b, &key(3)).unwrap().unwrap()["gamma"],
        Variant::Str("z".into())
    );
}

/// One name written under two keys resolves once, and a collection with an EMPTY
/// mirror (a fresh handle, as after a restart) still reads both documents back
/// by name — the vocabulary entry is durable, the mirror is only a shortcut.
#[test]
fn one_dynamic_name_across_documents_survives_a_fresh_mirror() {
    let store = TestStore::slatedb_mem();
    let t: Collection<TestStore, UserKey, User> = Collection::new(store.clone());
    t.put(&key(1), &user(1, 2)).unwrap();
    t.put(&key(2), &user(1, 3)).unwrap();
    t.put(
        &key(1),
        &Variant::Map(dmap(&[("tag", Variant::Str("alpha".into()))])),
    )
    .unwrap();
    t.put(
        &key(2),
        &Variant::Map(dmap(&[("tag", Variant::Str("beta".into()))])),
    )
    .unwrap();

    assert_eq!(
        doc_of(&t, &key(2)).unwrap().unwrap()["tag"],
        Variant::Str("beta".into())
    );

    let fresh: Collection<TestStore, UserKey, User> = Collection::new(store.shared_handle());
    assert_eq!(
        doc_of(&fresh, &key(1)).unwrap().unwrap()["tag"],
        Variant::Str("alpha".into())
    );
    assert_eq!(
        doc_of(&fresh, &key(2)).unwrap().unwrap()["tag"],
        Variant::Str("beta".into())
    );
}
