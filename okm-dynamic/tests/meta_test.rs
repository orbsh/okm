//! Table meta on the dynamic side (ADR-0043): a runtime-declared table writes the
//! SAME `[ns][0x0F]` row the typed path writes.
//!
//! The point of the lock is the shared format, not a second implementation: the row
//! carries the caller's names and the declaration's face fingerprint, computed the
//! same way on both sides, so a store stamped by the derive satisfies
//! `DynamicCollection::ensure_layout` and the reverse.
//!
//! The payload's GENERATION is deliberately not in this row (ADR-0042 as amended):
//! it is the segment header's statement, checked where a log is opened, because a
//! row that must be decoded before it can be trusted cannot vouch for its own bytes.
//!
//! Locks: byte equality across both modes, a caller name beside the fingerprint, and
//! the entry's exclusion from dynamic scans.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::schema::CollectionSchema;
use okm_core::{Document, DocumentEncode, KeyEncode, VirtualStorage, meta};
use okm_dynamic::{Dict, DynamicCollection, Value};

use okm_async::TestStore;
/// ns 92 = `[0x00, 0x5C]` — the hex locks below spell it out.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct MetaKey {
    pub id: u32,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MetaKey)]
#[ok_ns(92)]
#[ok_layout(version = 3)]
#[ok_name("dynamic_meta_probe")]
pub struct MetaRow {
    pub label: String,
}

fn header() -> Vec<u8> {
    92u16.to_be_bytes().to_vec()
}

fn dynamic<S: VirtualStorage>(store: S) -> DynamicCollection<S> {
    DynamicCollection::new(
        store,
        92,
        CollectionSchema::of::<MetaKey, MetaRow>(),
        Vec::new(),
    )
}

fn document(label: &str) -> Dict {
    let mut m = Dict::new();
    m.insert("label".into(), Value::Str(label.into()));
    m
}

#[test]
fn the_dynamic_stamp_is_the_row_the_typed_path_writes() {
    let typed_store = TestStore::default();
    let typed = <MetaRow as Document>::collection(typed_store.clone());
    let typed_meta = typed.ensure_layout().unwrap();
    assert!(
        typed_meta.faces().is_some(),
        "one name, written by the declaration itself"
    );

    let dynamic_store = TestStore::default();
    let table = dynamic(dynamic_store.clone());
    let dynamic_meta = table.ensure_layout().unwrap();

    assert_eq!(dynamic_meta, typed_meta, "one format, two modes");
    let key = meta::meta_key(&header());
    assert_eq!(key, vec![0x00, 0x5C, 0x0F], "`[ns 2B][0x0F]`, no suffix");
    assert_eq!(
        dynamic_store.get(&key).unwrap().unwrap(),
        typed_store.get(&key).unwrap().unwrap(),
        "the payload bytes are the same, not merely equivalent",
    );
}

#[test]
fn either_mode_answers_the_other_modes_stamp() {
    let store = TestStore::default();
    dynamic(store.clone()).ensure_layout().unwrap();
    let typed = <MetaRow as Document>::collection(store.clone());
    assert_eq!(
        typed.ensure_layout().unwrap().faces(),
        Some(MetaRow::FACES),
        "the fingerprint the dynamic side computed is the declaration's own"
    );

    let store = TestStore::default();
    let typed = <MetaRow as Document>::collection(store.clone());
    typed.ensure_layout().unwrap();
    assert_eq!(
        dynamic(store.clone()).ensure_layout().unwrap().faces(),
        Some(MetaRow::FACES)
    );
}

#[test]
fn a_caller_name_rides_beside_the_fingerprint() {
    let store = TestStore::default();
    let table = dynamic(store.clone());

    // The first-touch verb writes the fingerprint on a store that has no row.
    let stamped = table.ensure_layout().unwrap();
    assert_eq!(stamped.faces(), Some(MetaRow::FACES));

    // `set_meta` then adds the caller's name BESIDE it, next to the two names the
    // runtime owns (the fingerprint and the table's own name).
    let after = table.set_meta("build", b"20261030".to_vec()).unwrap();
    assert_eq!(after.get("build"), Some(&b"20261030"[..]));
    assert_eq!(after.faces(), Some(MetaRow::FACES));
    assert_eq!(table.meta().unwrap(), Some(after.clone()));
}

#[test]
fn a_second_name_at_one_ns_is_refused() {
    // Two runtime declarations of ns 92 with DIFFERENT names: the store, not the
    // compiler, is where they meet (a derive sees one module; a host declares these
    // at runtime, so nothing sees them together at all).
    let store = TestStore::default();
    let first = DynamicCollection::new(
        store.clone(),
        92,
        CollectionSchema::of::<MetaKey, MetaRow>(),
        Vec::new(),
    );
    first.ensure_layout().unwrap();

    let mut renamed = CollectionSchema::of::<MetaKey, MetaRow>();
    renamed.name = "another_table".into();
    let second = DynamicCollection::new(store.clone(), 92, renamed, Vec::new());

    let err = second.ensure_layout().unwrap_err();
    assert!(
        err.contains("already claimed by table `dynamic_meta_probe`")
            && err.contains("another_table")
            && err.contains("ns 92"),
        "the message names the ns and both tables: {err}"
    );

    // The refused declaration left the row as the first one stamped it.
    assert_eq!(
        first.meta().unwrap().unwrap().table_name(),
        Some("dynamic_meta_probe")
    );
}

#[test]
fn the_meta_entry_never_surfaces_in_a_dynamic_scan() {
    let store = TestStore::default();
    let table = dynamic(store.clone());
    table.ensure_layout().unwrap();

    let pkey = MetaKey { id: 7 }.encode();
    table.put(&pkey, &document("row")).unwrap();

    assert_eq!(table.scan_document_keys().unwrap(), vec![pkey.clone()]);
    assert_eq!(table.fetch(&pkey).unwrap().unwrap(), document("row"));
}
