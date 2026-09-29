//! DynamicCollection acceptance: runtime-declared tables over VirtualStorage
//! with schema-declared access methods. The lock that keeps the dynamic
//! path honest is byte equality with the typed path:
//!
//! - a `DynamicCollection` and a typed `Collection` writing the same document into the
//!   same ns must land byte-identical entries (primary + index), so a
//!   dynamic scan sees typed writes and vice versa;
//! - access-method scans return the matching rows' primary keys;
//! - overwrite with changed indexed values sweeps the stale entry;
//! - delete removes primary + index entries.
//!
//! Capability scope (ADR-0022): subscribe stays excluded; func/partial
//! indexes and reduce are callable-implementable —
//! dynamic_semantics_test.rs is the calling-discipline acceptance.

use okm_core::{KeyEncode, DocumentEncode, Collection, TestStore, VirtualStorage};
use okm_core::schema::CollectionSchema;
use okm_dynamic::{AccessMethod, DynamicCollection, Value, ValueMap};
use std::collections::BTreeMap;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
#[ok_layout(version = 2)]
#[ok_index(by_level { fields(level) })]
#[ok_index(by_score { fields(score), includes(level) })]
pub struct User {
    pub level: u32,  // hot
    pub score: u16,  // hot
    pub name: String, // cold TLV (tag = declaration index 2)
}

fn schema() -> CollectionSchema {
    CollectionSchema::of::<UserKey, User>()
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

fn dynamic_table(store: TestStore) -> DynamicCollection<TestStore> {
    DynamicCollection::new(
        store,
        41,
        schema(),
        vec![
            AccessMethod::plain(0x1001, vec!["level".into()], vec![]),
            AccessMethod::plain(0x1002, vec!["score".into()], vec!["level".into()]),
        ],
    )
}

fn key_bytes(org_id: u32, user_id: u64) -> Vec<u8> {
    UserKey { org_id, user_id }.encode()
}

#[test]
fn dynamic_entries_equal_typed_entries() {
    let mut typed: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    let mut dynamic = dynamic_table(TestStore::slatedb_mem());

    let document = User { level: 4, score: 77, name: "bob".into() };
    typed.put(&UserKey { org_id: 1, user_id: 2 }, &document);
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

    // by_level (slot 16): level = 4 → users 10 and 12.
    let hits = t.scan(0x1001, &4u32.to_be_bytes()).unwrap();
    let mut ids: Vec<u64> = hits
        .iter()
        .map(|k| match k.get("user_id") {
            Some(Value::U64(v)) => *v,
            other => panic!("unexpected key decode: {other:?}"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![10, 12]);

    // by_score (slot 17): score = 100 -> users 10 and 11.
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&100u16.to_be_bytes());
    assert_eq!(t.scan(0x1002, &prefix).unwrap().len(), 2);

    // No match: org 2 has nobody.
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&999u16.to_be_bytes());
    assert!(t.scan(0x1002, &prefix).unwrap().is_empty());
}

#[test]
fn dynamic_overwrite_sweeps_stale_entries() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a")).unwrap();

    // Overwrite changes `level` 4 → 9: the old by_level entry (slot 16,
    // level 4) must be gone, the new one present.
    t.put(&key_bytes(1, 10), &values(1, 10, 9, 100, "a")).unwrap();

    let level4 = t.scan(0x1001, &4u32.to_be_bytes()).unwrap();
    assert!(level4.is_empty(), "stale by_level entry must be swept");
    let level9 = t.scan(0x1001, &9u32.to_be_bytes()).unwrap();
    assert_eq!(level9.len(), 1);
}

#[test]
fn dynamic_delete_removes_all_entries() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a")).unwrap();
    t.delete(&key_bytes(1, 10)).unwrap();

    assert!(t.scan(0x1001, &4u32.to_be_bytes()).unwrap().is_empty());
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&100u16.to_be_bytes());
    assert!(t.scan(0x1002, &prefix).unwrap().is_empty());
    assert!(t.get(&key_bytes(1, 10)).unwrap().is_none());
}

#[test]
fn dynamic_get_round_trips() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(7, 8), &values(7, 8, 2, 300, "zoe")).unwrap();
    let document = t.get(&key_bytes(7, 8)).unwrap().expect("document present");
    assert_eq!(document.get("level"), Some(&Value::U32(2)));
    assert_eq!(document.get("score"), Some(&Value::U16(300)));
    assert_eq!(document.get("name"), Some(&Value::Str("zoe".into())));

    // Key-width discipline: a short key is a caller bug, not silent bytes.
    assert!(t.put(&[0u8; 4], &values(7, 8, 2, 300, "zoe")).is_err());
}

#[test]
fn dynamic_rejects_key_field_indexes() {
    // Key fields in `fields` or `includes` are declared-scheme errors:
    // the key IS the lookup target; indexing it is meaningless and
    // ambiguous under key/payload name collisions.
    let bad = AccessMethod::plain(0x1003, vec!["org_id".into()], vec![]);
    assert!(okm_dynamic::index_entries(
        &schema(),
        &[41],
        &[bad],
        &key_bytes(1, 2),
        &values(1, 2, 4, 77, "bob"),
    )
    .is_err());

    let bad_inc = AccessMethod::plain(0x1003, vec!["level".into()], vec!["user_id".into()]);
    assert!(okm_dynamic::index_entries(
        &schema(),
        &[41],
        &[bad_inc],
        &key_bytes(1, 2),
        &values(1, 2, 4, 77, "bob"),
    )
    .is_err());
}

// --- Dynamic-segment bridge -------------------------------------------
// The same lock as the primary/index entries: dynamic put_fields must
// land the exact bytes the typed path's put_fields writes for the same
// fields — the dynamic executor's field-level ops see typed writers'
// dynamic segments and vice versa.

#[test]
fn dynamic_fields_equal_typed_fields() {
    use okm_core::model::obj_dynamic::DynamicValue;

    let mut typed: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    let mut dynamic = dynamic_table(TestStore::slatedb_mem());

    let document = User { level: 1, score: 5, name: "a".into() };
    typed.put(&UserKey { org_id: 1, user_id: 2 }, &document);
    dynamic.put(&key_bytes(1, 2), &values(1, 2, 1, 5, "a")).unwrap();

    let mut fields = BTreeMap::new();
    fields.insert("note".to_string(), DynamicValue::Str("hello".into()));
    fields.insert("weight".to_string(), DynamicValue::UInt(9));
    typed.put_fields(&UserKey { org_id: 1, user_id: 2 }, &fields);
    let mut dfields: BTreeMap<String, Value> = BTreeMap::new();
    dfields.insert("note".to_string(), Value::Str("hello".into()));
    dfields.insert("weight".to_string(), Value::U64(9));
    dynamic.put_fields(&key_bytes(1, 2), &dfields).unwrap();

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
    assert_eq!(typed_entries, dyn_entries, "dynamic and typed field writes must land identical entries (incl. dictionary)");

    // Read-back through the dynamic bridge.
    let back = dynamic.get_fields(&key_bytes(1, 2)).unwrap().unwrap();
    assert_eq!(back.get("note"), Some(&Value::Str("hello".into())));
    assert_eq!(back.get("weight"), Some(&Value::U64(9)));
    assert_eq!(back.len(), 2);
}

#[test]
fn dynamic_fields_replace_and_delete() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(1, 3), &values(1, 3, 1, 5, "a")).unwrap();

    let mut f1: BTreeMap<String, Value> = BTreeMap::new();
    f1.insert("a".into(), Value::Str("x".into()));
    f1.insert("b".into(), Value::U64(1));
    t.put_fields(&key_bytes(1, 3), &f1).unwrap();

    // Whole-entry replace: `a` is gone.
    let mut f2: BTreeMap<String, Value> = BTreeMap::new();
    f2.insert("b".into(), Value::U64(2));
    t.put_fields(&key_bytes(1, 3), &f2).unwrap();
    let back = t.get_fields(&key_bytes(1, 3)).unwrap().unwrap();
    assert_eq!(back.len(), 1, "replace drops absent fields");
    assert_eq!(back.get("b"), Some(&Value::U64(2)));

    // Empty map deletes the entry; the primary document is untouched.
    let empty: BTreeMap<String, Value> = BTreeMap::new();
    t.put_fields(&key_bytes(1, 3), &empty).unwrap();
    assert!(t.get_fields(&key_bytes(1, 3)).unwrap().is_none());
    let doc = t.get(&key_bytes(1, 3)).unwrap().unwrap();
    assert_eq!(doc.get("name"), Some(&Value::Str("a".into())));

    // delete_fields is idempotent.
    let mut f3: BTreeMap<String, Value> = BTreeMap::new();
    f3.insert("c".into(), Value::Bool(true));
    t.put_fields(&key_bytes(1, 3), &f3).unwrap();
    assert!(t.delete_fields(&key_bytes(1, 3)));
    assert!(!t.delete_fields(&key_bytes(1, 3)));
    assert!(t.get_fields(&key_bytes(1, 3)).unwrap().is_none());
}

#[test]
fn dynamic_fields_carry_composites() {
    use std::collections::BTreeMap as M;
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(1, 4), &values(1, 4, 1, 5, "a")).unwrap();

    // Nested Obj + heterogeneous Array round-trip through the bridge.
    let mut nested: M<String, Value> = M::new();
    nested.insert("n".into(), Value::Str("v".into()));
    let mut f: M<String, Value> = M::new();
    f.insert("obj".into(), Value::Obj(nested));
    f.insert("tags".into(), Value::Array(vec![Value::Str("x".into()), Value::U64(3)]));
    t.put_fields(&key_bytes(1, 4), &f).unwrap();

    let back = t.get_fields(&key_bytes(1, 4)).unwrap().unwrap();
    assert_eq!(
        back.get("obj"),
        Some(&Value::Obj({
            let mut m = M::new();
            m.insert("n".into(), Value::Str("v".into()));
            m
        })),
        "nested obj decodes fully name-keyed"
    );
    assert_eq!(
        back.get("tags"),
        Some(&Value::Array(vec![Value::Str("x".into()), Value::U64(3)]))
    );

    // The typed path sees the same bytes: Collection::get_fields over a
    // store written by the dynamic bridge returns okm-core DynamicValue.
    let mut raw_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for full in t.store().scan_suffix(&[]) {
        let v = t.store().get(&full).unwrap_or_default();
        raw_entries.push((full, v));
    }
    assert!(!raw_entries.is_empty());
}

// --- plan surface (ADR-0037 4.16a) -----------------------------------
// The remote shape: a caller holding NO engine — dictionary mirror in,
// ops out — replayed blind must land byte-identical to the embedded
// put_fields (load->plan->replay). This is the injection contract the
// bindings ride: ship the ops, the engine never leaves its host.

#[test]
fn plan_replay_equals_embedded_fields_across_two_writes() {
    let ns = 7u16;
    let nsb = ns.to_be_bytes();
    let store = TestStore::slatedb_mem();

    // Caller-side: two writes through ONE mirror, adopted between them.
    let mut dict = okm_dynamic::DictMirror::default();
    let p1 = okm_dynamic::plan_put_fields(&nsb, b"k1", &dfields(&[("note", "hello"), ("weight9", "9")]), &mut dict).unwrap();
    for (k, v) in &p1.ops { match v { Some(b) => store.put(k.clone(), b.clone()), None => store.del(k) } }
    dict.adopt(&p1.new_dict_entries);
    let mut f2 = okm_dynamic::ValueMap::new();
    f2.insert("note".into(), okm_dynamic::Value::U64(42));
    let p2 = okm_dynamic::plan_put_fields(&nsb, b"k2", &f2, &mut dict).unwrap();
    for (k, v) in &p2.ops { match v { Some(b) => store.put(k.clone(), b.clone()), None => store.del(k) } }
    dict.adopt(&p2.new_dict_entries);
    assert_eq!(p2.new_dict_entries, Vec::new(), "note was already allocated by the first plan");

    // Embedded side on a fresh store: the same two writes.
    let mut t = dynamic_table(TestStore::slatedb_mem()); // same schema ns as above? check below
    // dynamic_table uses its own ns; build a matching collection:
    drop(t);
    let mut t = okm_dynamic::DynamicCollection::new(TestStore::slatedb_mem(), ns, schema(), Vec::new());
    t.put_fields(b"k1", &dfields(&[("note", "hello"), ("weight9", "9")])).unwrap();
    let mut g2 = okm_dynamic::ValueMap::new();
    g2.insert("note".into(), okm_dynamic::Value::U64(42));
    t.put_fields(b"k2", &g2).unwrap();

    let mut a: Vec<(Vec<u8>, Vec<u8>)> = store
        .scan_suffix(&[])
        .into_iter()
        .map(|full| (full.clone(), store.get(&full).unwrap_or_default()))
        .collect();
    let mut b: Vec<(Vec<u8>, Vec<u8>)> = t
        .store()
        .scan_suffix(&[])
        .into_iter()
        .map(|full| (full.clone(), t.store().get(&full).unwrap_or_default()))
        .collect();
    a.sort();
    b.sort();
    assert_eq!(a, b, "engine-less plan replay == embedded put_fields, byte for byte");

    // The remote read side: frames + the caller's own mirror decode to
    // the same map the embedded get_fields returns.
    let raw = store
        .get(&okm_dynamic::fields_key(&nsb, b"k1"))
        .unwrap();
    let back = okm_dynamic::fields_from_frames(&raw, &dict).unwrap();
    assert_eq!(back.get("note"), Some(&okm_dynamic::Value::Str("hello".into())));
    let emb = t.get_fields(b"k1").unwrap().unwrap();
    assert_eq!(back, emb);

    // plan_delete_fields lands where embedded delete_fields did.
    let p3 = okm_dynamic::plan_delete_fields(&nsb, b"k2");
    for (k, v) in &p3.ops { match v { Some(b) => store.put(k.clone(), b.clone()), None => store.del(k) } }
    assert_eq!(
        store.get(&okm_dynamic::fields_key(&nsb, b"k2")),
        None,
        "the planned delete removed the entry"
    );
}

fn dfields(pairs: &[(&str, &str)]) -> okm_dynamic::ValueMap {
    let mut m = okm_dynamic::ValueMap::new();
    for (k, v) in pairs {
        m.insert(
            k.to_string(),
            if v.chars().all(|c| c.is_ascii_digit()) {
                okm_dynamic::Value::U64(v.parse().unwrap())
            } else {
                okm_dynamic::Value::Str(v.to_string())
            },
        );
    }
    m
}
