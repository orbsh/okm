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

use okm_core::engine::storage::ScanIter;
use okm_core::{
    Collection, DocumentEncode, KeyEncode, Batch, TestStore, VirtualStorage,
};
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

/// The PAYLOAD names only: the identity is the pkey's business, and a map
/// naming a key field would be writing a DYNAMIC field of that name (ADR-0035:
/// the classifier is the payload declaration, not the schema's whole name set).
fn values(level: u32, score: u16, name: &str) -> ValueMap {
    let mut m = BTreeMap::new();
    m.insert("level".into(), Value::U32(level));
    m.insert("score".into(), Value::U16(score));
    m.insert("name".into(), Value::Str(name.into()));
    m
}

fn dynamic_table<S: VirtualStorage>(store: S) -> DynamicCollection<S> {
    DynamicCollection::new(
        store,
        41,
        schema(),
        vec![
            AccessMethod::plain(0x3001, vec!["level".into()], vec![]),
            AccessMethod::plain(0x3002, vec!["score".into()], vec!["level".into()]),
        ],
    )
}

fn key_bytes(org_id: u32, user_id: u64) -> Vec<u8> {
    UserKey { org_id, user_id }.encode()
}

/// An engine handle that records POINT READS: the lock for "a verb reads only
/// the stored state its own arm consumes" (the schema classification costs no
/// read). The vocabulary cursor probe is a read too, so assertions ask which
/// keys were touched, never how many.
struct Reads<S> {
    inner: S,
    read: std::cell::RefCell<Vec<Vec<u8>>>,
}

impl<S> Reads<S> {
    fn new(inner: S) -> Self {
        Self { inner, read: std::cell::RefCell::new(Vec::new()) }
    }

    fn reset(&self) {
        self.read.borrow_mut().clear();
    }

    fn took(&self, key: &[u8]) -> bool {
        self.read.borrow().iter().any(|k| k == key)
    }
}

impl<S: VirtualStorage> VirtualStorage for Reads<S> {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        self.inner.put(key, value)
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.read.borrow_mut().push(key.to_vec());
        self.inner.get(key)
    }

    fn del(&self, key: &[u8]) {
        self.inner.del(key)
    }

    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.inner.scan_suffix(prefix)
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        self.inner.scan_range(begin, end)
    }

    fn scan_range_iter(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        self.inner.scan_range_iter(begin, end)
    }

    fn commit_batch(&self, batch: Batch) -> Result<(), String> {
        self.inner.commit_batch(batch)
    }
}

/// The two slot-0/slot-1 keys of a document, derived the way the collection
/// derives them (primary = `[ns][0x00][pkey]`, segment = `[ns][0x01][pkey]`).
fn row_key(org_id: u32, user_id: u64) -> Vec<u8> {
    let mut k = 41u16.to_be_bytes().to_vec();
    okm_core::write_slot(&mut k, okm_core::model::index::PRIMARY_SLOT);
    k.extend_from_slice(&key_bytes(org_id, user_id));
    k
}

fn segment_key(org_id: u32, user_id: u64) -> Vec<u8> {
    okm_dynamic::fields_key(&41u16.to_be_bytes(), &key_bytes(org_id, user_id))
}

#[test]
fn dynamic_entries_equal_typed_entries() {
    let typed: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    let dynamic = dynamic_table(TestStore::slatedb_mem());

    let document = User { level: 4, score: 77, name: "bob".into() };
    typed.put(&UserKey { org_id: 1, user_id: 2 }, &document).unwrap();
    dynamic
        .merge(&key_bytes(1, 2), &values(4, 77, "bob"))
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
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 10), &values(4, 100, "a")).unwrap();
    t.merge(&key_bytes(1, 11), &values(9, 100, "b")).unwrap();
    t.merge(&key_bytes(1, 12), &values(4, 200, "c")).unwrap();

    // by_level (slot 16): level = 4 → users 10 and 12.
    let hits = t.scan(0x3001, &4u32.to_be_bytes()).unwrap();
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
    assert_eq!(t.scan(0x3002, &prefix).unwrap().len(), 2);

    // No match: org 2 has nobody.
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&999u16.to_be_bytes());
    assert!(t.scan(0x3002, &prefix).unwrap().is_empty());
}

#[test]
fn dynamic_covered_scan_answers_without_fetching_documents() {
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 10), &values(4, 100, "a")).unwrap();
    t.merge(&key_bytes(1, 11), &values(9, 100, "b")).unwrap();
    t.merge(&key_bytes(1, 12), &values(4, 200, "c")).unwrap();

    // by_score (slot 0x3002) declares includes(level): the entry VALUE carries
    // it, so the covered scan answers `level` per hit without fetching a doc.
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&100u16.to_be_bytes());
    let covered = t.scan_covered(0x3002, &prefix).unwrap();
    assert_eq!(covered.len(), 2);
    let mut got: Vec<(u64, u32)> = covered
        .iter()
        .map(|(k, c)| match (k.get("user_id"), c.get("level")) {
            (Some(Value::U64(id)), Some(Value::U32(level))) => (*id, *level),
            other => panic!("unexpected covered decode: {other:?}"),
        })
        .collect();
    got.sort();
    assert_eq!(got, vec![(10, 4), (11, 9)]);

    // A declaration without includes covers nothing: the map is empty, and the
    // call is still a normal (error-free) scan.
    let covered = t.scan_covered(0x3001, &4u32.to_be_bytes()).unwrap();
    assert_eq!(covered.len(), 2);
    assert!(covered.iter().all(|(_, c)| c.is_empty()));
}

#[test]
fn dynamic_covered_str_include_decodes_the_trailing_remainder() {
    // A Str include is the variable-width tail: raw UTF-8 with no length
    // prefix, so the decoder takes the remainder — per entry, not per index.
    let t = DynamicCollection::new(
        TestStore::slatedb_mem(),
        41,
        schema(),
        vec![AccessMethod::plain(
            0x3001,
            vec!["level".into()],
            vec!["name".into()],
        )],
    );
    t.merge(&key_bytes(1, 10), &values(4, 100, "短")).unwrap();
    t.merge(&key_bytes(1, 11), &values(4, 100, "更长的名字")).unwrap();

    let covered = t.scan_covered(0x3001, &4u32.to_be_bytes()).unwrap();
    assert_eq!(covered.len(), 2);
    let names: Vec<&str> = covered
        .iter()
        .map(|(_, c)| match c.get("name") {
            Some(Value::Str(s)) => s.as_str(),
            other => panic!("unexpected covered decode: {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["短", "更长的名字"]);
}

#[test]
fn dynamic_overwrite_sweeps_stale_entries() {
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 10), &values(4, 100, "a")).unwrap();

    // Overwrite changes `level` 4 → 9: the old by_level entry (slot 16,
    // level 4) must be gone, the new one present.
    t.merge(&key_bytes(1, 10), &values(9, 100, "a")).unwrap();

    let level4 = t.scan(0x3001, &4u32.to_be_bytes()).unwrap();
    assert!(level4.is_empty(), "stale by_level entry must be swept");
    let level9 = t.scan(0x3001, &9u32.to_be_bytes()).unwrap();
    assert_eq!(level9.len(), 1);
}

#[test]
fn dynamic_purge_removes_all_entries() {
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 10), &values(4, 100, "a")).unwrap();
    t.purge(&key_bytes(1, 10)).unwrap();

    assert!(t.scan(0x3001, &4u32.to_be_bytes()).unwrap().is_empty());
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&100u16.to_be_bytes());
    assert!(t.scan(0x3002, &prefix).unwrap().is_empty());
    assert!(t.fetch(&key_bytes(1, 10)).unwrap().is_none());
}

#[test]
fn dynamic_get_round_trips() {
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(7, 8), &values(2, 300, "zoe")).unwrap();
    let document = t.fetch(&key_bytes(7, 8)).unwrap().expect("document present");
    assert_eq!(document.get("level"), Some(&Value::U32(2)));
    assert_eq!(document.get("score"), Some(&Value::U16(300)));
    assert_eq!(document.get("name"), Some(&Value::Str("zoe".into())));

    // Key-width discipline: a short key is a caller bug, not silent bytes.
    assert!(t.merge(&[0u8; 4], &values(2, 300, "zoe")).is_err());
}

#[test]
fn a_verb_reads_only_the_state_its_arm_consumes() {
    // The schema classification is free (the schema is in memory): a map of
    // dynamic names must never read the row, a map of declared names must never
    // read the segment. So `merge` is not "one read more" than the retired
    // `put` — `put` read the row it was about to rewrite, and so does the
    // declared arm.
    let t = dynamic_table(Reads::new(TestStore::slatedb_mem()));
    let (row, seg) = (row_key(1, 2), segment_key(1, 2));

    // Dynamic names only → the segment, never the row.
    t.merge(&key_bytes(1, 2), &dfields(&[("note", "x")])).unwrap();
    assert!(!t.store().took(&row), "dynamic-only merge reads no row");
    assert!(t.store().took(&seg), "dynamic-only merge reads the segment");

    // Declared names only → the row, never the segment.
    t.store().reset();
    t.merge(&key_bytes(1, 2), &values(4, 77, "bob")).unwrap();
    assert!(t.store().took(&row), "declared-only merge reads the row");
    assert!(!t.store().took(&seg), "declared-only merge reads no segment");

    // Both kinds → both, which is the only case that pays two reads.
    t.store().reset();
    let mut both = values(4, 77, "bob");
    both.insert("note".into(), Value::Str("y".into()));
    t.merge(&key_bytes(1, 2), &both).unwrap();
    assert!(t.store().took(&row) && t.store().took(&seg), "mixed merge reads both");

    // `replace` always reads the row (rebuild or delete — only the stored row
    // says which), and the segment only for a path index's sweep: this table
    // declares none, so the segment is not read.
    t.store().reset();
    t.replace(&key_bytes(1, 2), &values(4, 77, "bob")).unwrap();
    assert!(t.store().took(&row), "replace reads the row");
    assert!(!t.store().took(&seg), "no path index → replace reads no segment");

    // The read door and the removal verb touch both slots.
    t.store().reset();
    t.fetch(&key_bytes(1, 2)).unwrap();
    assert!(t.store().took(&row) && t.store().took(&seg), "fetch reads both");
    t.store().reset();
    assert!(t.purge(&key_bytes(1, 2)).unwrap());
    assert!(t.store().took(&row) && t.store().took(&seg), "purge reads both");
}

#[test]
fn an_undeclared_name_is_a_dynamic_name_on_this_side() {
    // There is no declared-only door here: the dynamic side has no struct, so a
    // name outside the schema is a dynamic name — the segment's normal
    // population, not a caller bug. The typed side keeps the struct (`put(&R)`)
    // and therefore cannot express the mistake at all (ADR-0035).
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 2), &values(4, 77, "bob")).unwrap();
    t.merge(&key_bytes(1, 2), &dfields(&[("extra", "x")])).unwrap();
    let back = t.fetch(&key_bytes(1, 2)).unwrap().unwrap();
    assert_eq!(back.get("extra"), Some(&Value::Str("x".into())));
    assert_eq!(back.get("name"), Some(&Value::Str("bob".into())));
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
        &values(4, 77, "bob"),
    )
    .is_err());

    let bad_inc = AccessMethod::plain(0x1003, vec!["level".into()], vec!["user_id".into()]);
    assert!(okm_dynamic::index_entries(
        &schema(),
        &[41],
        &[bad_inc],
        &key_bytes(1, 2),
        &values(4, 77, "bob"),
    )
    .is_err());
}

// --- Whole-document bridge (ADR-0035) --------------------------------
// The dynamic side speaks the typed side's one-map surface: `view` reads both
// slots, `merge` writes only the names it is given (a declared name is a row
// read-modify-write, any other name rides the dynamic segment), `replace`
// treats the map as the document, `purge` removes both slots. The trio that
// addressed slot 1 directly (`get_fields` / `put_fields` / `delete_fields`)
// is retired with no aliases.
//
// The lock is unchanged from the trio's day: a dynamic write must land the
// exact bytes the typed path's write lands, so either side sees the other.

#[test]
fn dynamic_fields_equal_typed_fields() {
    use okm_core::model::obj_dynamic::DynamicValue;

    let typed: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    let dynamic = dynamic_table(TestStore::slatedb_mem());

    let document = User { level: 1, score: 5, name: "a".into() };
    typed.put(&UserKey { org_id: 1, user_id: 2 }, &document).unwrap();
    dynamic.merge(&key_bytes(1, 2), &values(1, 5, "a")).unwrap();

    // One map, both modes: no declared name is named here, so both sides
    // write slot 1 only and the row is untouched.
    let mut fields = BTreeMap::new();
    fields.insert("note".to_string(), DynamicValue::Str("hello".into()));
    fields.insert("weight".to_string(), DynamicValue::UInt(9));
    typed
        .merge(&UserKey { org_id: 1, user_id: 2 }, &fields)
        .unwrap();
    dynamic
        .merge(&key_bytes(1, 2), &dfields(&[("note", "hello"), ("weight", "9")]))
        .unwrap();

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
    assert_eq!(typed_entries, dyn_entries, "dynamic and typed field writes must land identical entries (incl. vocabulary)");

    // Read-back through the merged projection: the row AND the segment.
    let back = dynamic.fetch(&key_bytes(1, 2)).unwrap().unwrap();
    assert_eq!(back.get("note"), Some(&Value::Str("hello".into())));
    assert_eq!(back.get("weight"), Some(&Value::U64(9)));
    assert_eq!(back.get("name"), Some(&Value::Str("a".into())), "view carries the row too");
}

#[test]
fn merge_keeps_the_unnamed_dynamic_name_and_never_writes_the_row() {
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 3), &values(1, 5, "a")).unwrap();
    t.merge(&key_bytes(1, 3), &dfields(&[("a", "x"), ("b", "1")]))
        .unwrap();

    // A second merge naming only `b` leaves `a` standing (ADR-0035 rule 3).
    let mut only_b = ValueMap::new();
    only_b.insert("b".into(), Value::U64(2));
    t.merge(&key_bytes(1, 3), &only_b).unwrap();

    let back = t.fetch(&key_bytes(1, 3)).unwrap().unwrap();
    assert_eq!(back.get("a"), Some(&Value::Str("x".into())), "merge does not drop unnamed dynamic names");
    assert_eq!(back.get("b"), Some(&Value::U64(2)));
    assert_eq!(
        back.get("name"),
        Some(&Value::Str("a".into())),
        "a map with no declared name never writes the row"
    );
}

#[test]
fn replace_is_the_whole_document_and_empty_is_purge() {
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 4), &values(1, 5, "a")).unwrap();
    t.merge(&key_bytes(1, 4), &dfields(&[("a", "x"), ("b", "1")]))
        .unwrap();

    // A map with NO declared name leaves the static segment absent: the
    // segment is the whole document (ADR-0035 rule 4).
    t.replace(&key_bytes(1, 4), &dfields(&[("b", "1")])).unwrap();
    let only_dynamic = t
        .fetch(&key_bytes(1, 4))
        .unwrap()
        .expect("a dynamic-only document still fetches");
    assert_eq!(
        only_dynamic.len(),
        1,
        "no declared name in the map → no row: the fetch is exactly the segment"
    );
    assert!(only_dynamic.contains_key("b"), "the map's dynamic name survives");

    // The declared map rebuilds the row and empties the segment.
    t.replace(&key_bytes(1, 4), &values(9, 6, "z")).unwrap();
    let row = t.fetch(&key_bytes(1, 4)).unwrap().unwrap();
    assert_eq!(row.get("level"), Some(&Value::U32(9)));
    assert_eq!(row.get("name"), Some(&Value::Str("z".into())));
    assert_eq!(
        t.fetch(&key_bytes(1, 4)).unwrap().unwrap().len(),
        3,
        "the segment is empty: the view is the row"
    );

    // replace(&empty) == purge.
    let empty = ValueMap::new();
    t.replace(&key_bytes(1, 4), &empty).unwrap();
    assert!(t.fetch(&key_bytes(1, 4)).unwrap().is_none());
}

#[test]
fn purge_is_the_one_deletion_verb_and_takes_both_slots() {
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 5), &values(1, 5, "a")).unwrap();
    t.merge(&key_bytes(1, 5), &dfields(&[("note", "kept")]))
        .unwrap();

    // The document is the PAIR of slots, so one verb removes it: row, faces,
    // ledger and the dynamic entry. A second purge has nothing left.
    assert!(t.purge(&key_bytes(1, 5)).unwrap());
    assert!(t.fetch(&key_bytes(1, 5)).unwrap().is_none());
    assert!(t.fetch(&key_bytes(1, 5)).unwrap().is_none());
    assert!(!t.purge(&key_bytes(1, 5)).unwrap());

    // A dynamic-only document (replace with no declared name) purges too —
    // that state is reachable through `replace`, and purge is what clears it.
    t.replace(&key_bytes(1, 5), &dfields(&[("note", "orphan")])).unwrap();
    assert!(t.fetch(&key_bytes(1, 5)).unwrap().is_some());
    assert!(t.purge(&key_bytes(1, 5)).unwrap());
    assert!(t.fetch(&key_bytes(1, 5)).unwrap().is_none());
}

#[test]
fn dynamic_fields_carry_composites() {
    use std::collections::BTreeMap as M;
    let t = dynamic_table(TestStore::slatedb_mem());
    t.merge(&key_bytes(1, 6), &values(1, 5, "a")).unwrap();

    // Nested Obj + heterogeneous Array round-trip through the bridge.
    let mut nested: M<String, Value> = M::new();
    nested.insert("n".into(), Value::Str("v".into()));
    let mut f: M<String, Value> = M::new();
    f.insert("obj".into(), Value::Obj(nested));
    f.insert("tags".into(), Value::Array(vec![Value::Str("x".into()), Value::U64(3)]));
    t.merge(&key_bytes(1, 6), &f).unwrap();

    let back = t.fetch(&key_bytes(1, 6)).unwrap().unwrap();
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

    // The reader sees them as declared fields would be seen: the row is
    // still there, untouched by the dynamic write.
    assert_eq!(back.get("name"), Some(&Value::Str("a".into())));
}

// --- plan surface (ADR-0037 4.16a) -----------------------------------
// The remote shape: a caller holding NO engine — the two old halves plus the
// vocabulary mirror in, ONE op list out — replayed blind must land
// byte-identical to the embedded merge (one list, one replay, one commit).
// This is the injection contract the bindings ride.

#[test]
fn plan_merge_equals_embedded_merge() {
    let ns = 41u16;
    let nsb = ns.to_be_bytes();
    let k = key_bytes(1, 2);
    let store = TestStore::slatedb_mem();
    let planner = dynamic_table(TestStore::slatedb_mem()); // schema only; its store is never touched

    let mut vocab = okm_dynamic::VocabMirror::default();
    let p1 = planner
        .plan_merge(
            &k,
            // Every name here is dynamic on this table, so the declared half is
            // empty — the same split the collection makes before planning.
            &okm_dynamic::ValueMap::new(),
            &dfields(&[("note", "hello"), ("weight", "9")]),
            okm_dynamic::Stored::default(),
            &|_ek: &[u8]| None,
            &mut vocab,
        )
        .unwrap();
    for (key, v) in &p1.ops {
        match v {
            Some(b) => store.put(key.clone(), b.clone()),
            None => store.del(key),
        }
    }
    vocab.adopt(&p1.new_vocab_entries);

    let emb = dynamic_table(TestStore::slatedb_mem());
    emb.merge(&k, &dfields(&[("note", "hello"), ("weight", "9")]))
        .unwrap();

    let mut planned: Vec<(Vec<u8>, Vec<u8>)> = store
        .scan_suffix(&[])
        .into_iter()
        .map(|full| (full.clone(), store.get(&full).unwrap_or_default()))
        .collect();
    let mut embedded: Vec<(Vec<u8>, Vec<u8>)> = emb
        .store()
        .scan_suffix(&[])
        .into_iter()
        .map(|full| (full.clone(), emb.store().get(&full).unwrap_or_default()))
        .collect();
    planned.sort();
    embedded.sort();
    assert_eq!(planned, embedded, "engine-less plan replay == embedded merge, byte for byte");

    // The remote read side: frames + the caller's own mirror decode to the
    // same dynamic half the embedded path reads.
    let raw = store.get(&okm_dynamic::fields_key(&nsb, &k)).unwrap();
    let back = okm_dynamic::fields_from_frames(&raw, &vocab).unwrap();
    assert_eq!(back, dfields(&[("note", "hello"), ("weight", "9")]));
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

// --- Variable-width trailing index field (the okm-derive rule, aligned) --

// The dynamic mode now accepts a variable-width (Str) field as the LAST
// index field: raw UTF-8, no length prefix (text-first), the fixed-width
// pkey tail cuts the segment. Byte-equality with the typed path holds for
// this shape too, scans match by equality probe, and the declaration
// errors (variable-width not last, two of them) mirror the derive's.

#[test]
fn dynamic_variable_width_index_matches_typed_bytes() {
    // Typed declaration with the SAME shape the derive accepts:
    // variable-width `name` last.
    #[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
    pub struct EmailKey {
        pub id: u64,
    }
    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(EmailKey)]
    #[ok_ns(43)]
    #[ok_index(by_email { fields(email) })]
    pub struct Subscriber {
        pub email: String, // cold TLV in the payload; raw UTF-8 in the index
    }

    let typed: Collection<TestStore, EmailKey, Subscriber> = Collection::new(TestStore::slatedb_mem());
    let schema = CollectionSchema::of::<EmailKey, Subscriber>();
    let dynamic = DynamicCollection::new(
        TestStore::slatedb_mem(),
        43,
        schema.clone(),
        vec![AccessMethod::plain(0x3001, vec!["email".into()], vec![])],
    );

    let doc = |email: &str| Subscriber { email: email.into() };
    let put = |c: &mut Subscriber, email: &str| c.email = email.into();
    let _ = put;

    for (id, email) in [(1u64, "a@x"), (2, "a@x"), (3, "b@x")] {
        typed.put(&EmailKey { id }, &doc(email)).unwrap();
        // Payload names only: the identity rides the pkey. Naming a key field
        // here would be a dynamic field of that name, not the identity (see
        // `a_key_field_name_in_the_map_is_a_dynamic_field`).
        let mut m = BTreeMap::new();
        m.insert("email".to_string(), Value::Str(email.into()));
        dynamic.merge(&EmailKey { id }.encode(), &m).expect("dynamic put");
    }

    // The real lock: identical entry sets.
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
    assert_eq!(typed_entries, dyn_entries, "variable-width index entries must be byte-identical across modes");

    // Equality probe with the FULL string (longer than nothing — the
    // pkey tail cuts the raw UTF-8): rows 1 and 2 match, 3 does not.
    let hits = dynamic.scan(0x3001, b"a@x").expect("equality probe");
    let mut ids: Vec<u64> = hits
        .iter()
        .map(|k| match k.get("id") {
            Some(Value::U64(v)) => *v,
            other => panic!("unexpected key decode: {other:?}"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2], "the fan-out: two rows share one email");

    // Overwrite sweeps the stale entry (the email changed).
    let mut m = BTreeMap::new();
    m.insert("email".to_string(), Value::Str("a@x".into()));
    dynamic.merge(&EmailKey { id: 3 }.encode(), &m).unwrap();
    let hits = dynamic.scan(0x3001, b"a@x").unwrap();
    let mut ids: Vec<u64> = hits
        .iter()
        .filter_map(|k| match k.get("id") {
            Some(Value::U64(v)) => Some(*v),
            _ => None,
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2, 3], "the moved row's entry followed it");
}

#[test]
fn dynamic_variable_width_index_declaration_errors() {
    let fields_last_ok = AccessMethod::plain(0x1003, vec!["name".into()], vec![]);
    assert!(fields_last_ok.fields_width(&schema()).is_ok(), "trailing Str is legal");

    // Variable-width NOT last: nothing can locate fields after it.
    let bad_order = AccessMethod::plain(0x1003, vec!["name".into(), "level".into()], vec![]);
    assert!(bad_order.fields_width(&schema()).is_err());

    // Two variable-width fields: the second has nothing to cut against.
    // (Build a two-Str schema via the typed path.)
    #[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
    pub struct TwoStrKey {
        pub id: u64,
    }
    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(TwoStrKey)]
    #[ok_ns(44)]
    pub struct TwoStr {
        pub a: String,
        pub b: String,
    }
    let two_str_schema = CollectionSchema::of::<TwoStrKey, TwoStr>();
    let two_variable = AccessMethod::plain(0x3001, vec!["a".into(), "b".into()], vec![]);
    assert!(two_variable.fields_width(&two_str_schema).is_err());

    // Includes segments share the rule.
    let bad_inc = AccessMethod::plain(0x1003, vec!["level".into()], vec!["name".into(), "score".into()]);
    assert!(okm_dynamic::index_entries(
        &schema(),
        &[41],
        &[bad_inc],
        &key_bytes(1, 2),
        &values(4, 77, "bob"),
    )
    .is_err());

    // And the write path enforces it too (a Str field first in fields).
    let bad_write = AccessMethod::plain(0x1003, vec!["name".into(), "level".into()], vec![]);
    assert!(okm_dynamic::index_entries(
        &schema(),
        &[41],
        &[bad_write],
        &key_bytes(1, 2),
        &values(4, 77, "bob"),
    )
    .is_err());
}
