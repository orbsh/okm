//! DynamicTable acceptance: runtime-declared tables over VirtualStorage
//! with schema-declared access methods. The lock that keeps the dynamic
//! path honest is byte equality with the typed path:
//!
//! - a `DynamicTable` and a typed `Table` writing the same row into the
//!   same ns must land byte-identical entries (primary + index), so a
//!   dynamic scan sees typed writes and vice versa;
//! - access-method scans return the matching rows' primary keys;
//! - overwrite with changed indexed values sweeps the stale entry;
//! - delete removes primary + index entries.
//!
//! Capability ceiling (permanent): no reduce/subscribe/function indexes —
//! the dynamic rebuild would break exactly-once (ADR-0008).

use okm_core::{KeyEncode, Row, RowEncode, Table, TestStore, VirtualStorage};
use okm_core::schema::TableSchema;
use okm_dynamic::{AccessMethod, DynamicTable, Value, ValueMap};
use std::collections::BTreeMap;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(UserKey)]
#[kv_ns(41)]
#[kv_layout(version = 2)]
#[kv_index(by_level { fields(level) })]
#[kv_index(by_score { fields(score), includes(level) })]
pub struct User {
    pub level: u32,  // hot
    pub score: u16,  // hot
    pub name: String, // cold TLV (tag = declaration index 2)
}

fn schema() -> TableSchema {
    TableSchema::of::<UserKey, User>()
}

fn values(org_id: u32, user_id: u64, level: u32, score: u16, name: &str) -> ValueMap {
    let mut m = BTreeMap::new();
    m.insert("org_id".into(), Value::U32(org_id));
    m.insert("user_id".into(), Value::U64(user_id));
    m.insert("level".into(), Value::U32(level));
    m.insert("score".into(), Value::U16(score));
    m.insert("name".into(), Value::Str(name.into()));
    m
}

fn dynamic_table(store: TestStore) -> DynamicTable<TestStore> {
    DynamicTable::new(
        store,
        41,
        schema(),
        vec![
            AccessMethod { slot: 1, fields: vec!["level".into()], includes: vec![] },
            AccessMethod {
                slot: 2,
                fields: vec!["score".into()],
                includes: vec!["level".into()],
            },
        ],
    )
}

fn key_bytes(org_id: u32, user_id: u64) -> Vec<u8> {
    UserKey { org_id, user_id }.encode()
}

#[test]
fn dynamic_entries_equal_typed_entries() {
    let mut typed: Table<TestStore, UserKey, User> = Table::new(TestStore::slatedb_mem());
    let mut dynamic = dynamic_table(TestStore::slatedb_mem());

    let row = User { level: 4, score: 77, name: "bob".into() };
    typed.put(&UserKey { org_id: 1, user_id: 2 }, &row);
    dynamic
        .put(&key_bytes(1, 2), &values(1, 2, 4, 77, "bob"))
        .expect("dynamic put");

    // The real lock: typed store's full entry set == dynamic store's.
    let mut typed_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for full in typed.store().scan_suffix(&[]) {
        let v = typed.store().get(&full).unwrap_or_default();
        typed_entries.push((full, v));
    }
    typed_entries.sort();
    let mut dyn_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for full in dynamic.store().scan_suffix(&[]) {
        let v = dynamic.store().get(&full).unwrap_or_default();
        dyn_entries.push((full, v));
    }
    dyn_entries.sort();
    assert_eq!(typed_entries, dyn_entries, "dynamic and typed tables must land identical entries");
}

#[test]
fn dynamic_scan_finds_by_access_method() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a")).unwrap();
    t.put(&key_bytes(1, 11), &values(1, 11, 9, 100, "b")).unwrap();
    t.put(&key_bytes(1, 12), &values(1, 12, 4, 200, "c")).unwrap();

    // by_level (slot 1): level = 4 → users 10 and 12.
    let hits = t.scan(1, &4u32.to_be_bytes()).unwrap();
    let mut ids: Vec<u64> = hits
        .iter()
        .map(|k| match k.get("user_id") {
            Some(Value::U64(v)) => *v,
            other => panic!("unexpected key decode: {other:?}"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![10, 12]);

    // by_score (slot 2): score = 100 -> users 10 and 11.
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&100u16.to_be_bytes());
    assert_eq!(t.scan(2, &prefix).unwrap().len(), 2);

    // No match: org 2 has nobody.
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&999u16.to_be_bytes());
    assert!(t.scan(2, &prefix).unwrap().is_empty());
}

#[test]
fn dynamic_overwrite_sweeps_stale_entries() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a")).unwrap();

    // Overwrite changes `level` 4 → 9: the old by_level entry (slot 1,
    // level 4) must be gone, the new one present.
    t.put(&key_bytes(1, 10), &values(1, 10, 9, 100, "a")).unwrap();

    let level4 = t.scan(1, &4u32.to_be_bytes()).unwrap();
    assert!(level4.is_empty(), "stale by_level entry must be swept");
    let level9 = t.scan(1, &9u32.to_be_bytes()).unwrap();
    assert_eq!(level9.len(), 1);
}

#[test]
fn dynamic_delete_removes_all_entries() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a")).unwrap();
    t.delete(&key_bytes(1, 10)).unwrap();

    assert!(t.scan(1, &4u32.to_be_bytes()).unwrap().is_empty());
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&100u16.to_be_bytes());
    assert!(t.scan(2, &prefix).unwrap().is_empty());
    assert!(t.get(&key_bytes(1, 10)).unwrap().is_none());
}

#[test]
fn dynamic_get_round_trips() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(7, 8), &values(7, 8, 2, 300, "zoe")).unwrap();
    let row = t.get(&key_bytes(7, 8)).unwrap().expect("row present");
    assert_eq!(row.get("level"), Some(&Value::U32(2)));
    assert_eq!(row.get("score"), Some(&Value::U16(300)));
    assert_eq!(row.get("name"), Some(&Value::Str("zoe".into())));

    // Key-width discipline: a short key is a caller bug, not silent bytes.
    assert!(t.put(&[0u8; 4], &values(7, 8, 2, 300, "zoe")).is_err());
}

#[test]
fn dynamic_rejects_key_field_indexes() {
    // Key fields in `fields` or `includes` are declared-scheme errors:
    // the key IS the lookup target; indexing it is meaningless and
    // ambiguous under key/payload name collisions.
    let bad = AccessMethod { slot: 3, fields: vec!["org_id".into()], includes: vec![] };
    assert!(okm_dynamic::index_entries(
        &schema(),
        &[41],
        &[bad],
        &key_bytes(1, 2),
        &values(1, 2, 4, 77, "bob"),
    )
    .is_err());

    let bad_inc =
        AccessMethod { slot: 3, fields: vec!["level".into()], includes: vec!["user_id".into()] };
    assert!(okm_dynamic::index_entries(
        &schema(),
        &[41],
        &[bad_inc],
        &key_bytes(1, 2),
        &values(1, 2, 4, 77, "bob"),
    )
    .is_err());
}
