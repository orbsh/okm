//! Table meta on the dynamic side (ADR-0043): a runtime-declared table stamps
//! the SAME `[ns][0x0F]` row the typed path stamps.
//!
//! The point of the lock is the shared format, not a second implementation:
//! `CollectionSchema::layout_version` IS `Document::LAYOUT_VERSION`, so a store
//! stamped by the derive satisfies `DynamicCollection::ensure_layout` and the
//! reverse. Before this, only the typed assembly point stamped anything — a
//! dynamic table's store carried no self-description, and a shape mismatch
//! surfaced as a decode failure on the first row instead of a refusal at attach.
//!
//! Locks: byte equality of the stamp across both modes (and both read
//! directions), the two refusal shapes, a caller-owned name beside the
//! runtime's, and the entry's exclusion from dynamic scans.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::schema::CollectionSchema;
use okm_core::{Document, DocumentEncode, KeyEncode, TableMeta, TestStore, VirtualStorage, meta};
use okm_dynamic::{DynamicCollection, Value, ValueMap};

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

fn document(label: &str) -> ValueMap {
    let mut m = ValueMap::new();
    m.insert("label".into(), Value::Str(label.into()));
    m
}

#[test]
fn the_dynamic_stamp_is_the_row_the_typed_path_writes() {
    let typed_store = TestStore::default();
    let typed = <MetaRow as Document>::collection(typed_store.clone());
    let typed_meta = typed.ensure_layout().unwrap();
    assert_eq!(
        typed_meta.version(),
        Some(3),
        "schema.layout_version is the stamp"
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
    assert_eq!(typed.ensure_layout().unwrap().version(), Some(3));

    let store = TestStore::default();
    let typed = <MetaRow as Document>::collection(store.clone());
    typed.ensure_layout().unwrap();
    assert_eq!(
        dynamic(store.clone()).ensure_layout().unwrap().version(),
        Some(3)
    );
}

#[test]
fn a_store_stamped_with_another_shape_is_refused() {
    let store = TestStore::default();
    let table = dynamic(store.clone());
    meta::write(&store, &header(), &TableMeta::stamped(9)).unwrap();

    let err = table.ensure_layout().unwrap_err();
    assert!(err.contains("stamped Some(9)"), "{err}");
    assert!(err.contains("writes 3"), "{err}");
    assert!(err.contains("delete it"), "{err}");
}

#[test]
fn a_meta_row_that_names_no_version_is_refused() {
    let store = TestStore::default();
    let table = dynamic(store.clone());
    let mut foreign = TableMeta::new();
    foreign.set("whoever", b"not okm".to_vec());
    meta::write(&store, &header(), &foreign).unwrap();

    let err = table.ensure_layout().unwrap_err();
    assert!(err.contains("stamped None"), "{err}");
}

#[test]
fn a_caller_name_rides_beside_the_stamp() {
    let store = TestStore::default();
    let table = dynamic(store.clone());

    let after = table.set_meta("build", b"20261030".to_vec()).unwrap();
    assert_eq!(after.version(), Some(3), "the runtime re-stamps version");
    assert_eq!(after.get("build"), Some(&b"20261030"[..]));
    assert_eq!(table.meta().unwrap(), Some(after.clone()));

    // The stamp cannot be edited away through the caller's verb.
    let after = table.set_meta(meta::VERSION, vec![9]).unwrap();
    assert_eq!(after.version(), Some(3));
    assert!(table.ensure_layout().is_ok());
}

#[test]
fn the_meta_entry_never_surfaces_in_a_dynamic_scan() {
    let store = TestStore::default();
    let table = dynamic(store.clone());
    table.ensure_layout().unwrap();

    let pkey = MetaKey { id: 7 }.encode();
    table.merge(&pkey, &document("row")).unwrap();

    assert_eq!(table.scan_document_keys().unwrap(), vec![pkey.clone()]);
    assert_eq!(table.fetch(&pkey).unwrap().unwrap(), document("row"));
}
