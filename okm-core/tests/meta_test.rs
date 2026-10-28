//! Table meta (ADR-0043): fixed face 0x0F — ONE name → bytes entry per table,
//! describing the store instead of holding its data.
//!
//! Locks: the address (highest fixed face, no suffix), the shape stamp a fresh
//! store gets, the payload bytes of a version-only row, the kv form once a
//! second name exists, the two refusal shapes, and the entry's exclusion from
//! row/document scans.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::storage::VirtualStorage;
use okm_core::{
    meta, Document, DocumentEncode, Error, KeyEncode, TableMeta, TestStore,
};

/// ns 90 = `[0x00, 0x5A]` — the hex locks below spell it out.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct MetaKey {
    pub id: u32,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MetaKey)]
#[ok_ns(90)]
#[ok_name("meta_probe")]
pub struct MetaRow {
    pub label: String,
}

fn header() -> Vec<u8> {
    meta::document_header::<MetaRow>()
}

#[test]
fn the_stamp_lives_at_the_highest_fixed_face_with_no_suffix() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());

    let stamped = table.ensure_layout().unwrap();
    assert_eq!(stamped.version(), Some(1), "no #[ok_layout] = version 1");

    let key = meta::meta_key(&header());
    assert_eq!(key, vec![0x00, 0x5A, 0x0F], "`[ns 2B][0x0F]`, no suffix");

    // [count u16 BE] [name_len varint] [name] [value_len varint] [value]
    let payload = store.get(&key).unwrap().unwrap();
    assert_eq!(
        payload,
        vec![0x00, 0x01, 0x07, b'v', b'e', b'r', b's', b'i', b'o', b'n', 0x01, 0x01],
    );
}

#[test]
fn the_stamp_is_written_once_and_read_back_without_rewriting() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());
    let first = table.ensure_layout().unwrap();
    let second = table.ensure_layout().unwrap();
    assert_eq!(first, second);
    assert_eq!(table.meta().unwrap(), Some(first));
}

#[test]
fn a_second_name_rides_beside_the_stamp() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());

    let after = table.set_meta("build", b"20261028".to_vec()).unwrap();
    assert_eq!(after.version(), Some(1), "the runtime re-stamps version");
    assert_eq!(after.get("build"), Some(&b"20261028"[..]));
    assert_eq!(after.len(), 2);

    // Deterministic order: `build` sorts before `version`, and the row decodes
    // into the same map (round trip through the stored bytes).
    let stored = table.meta().unwrap().unwrap();
    assert_eq!(stored, after);
    assert_eq!(
        stored.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        vec!["build", "version"],
    );

    // Writing a caller value under the runtime's name does not replace the
    // stamp: `set_meta` re-stamps after applying it.
    let after = table.set_meta(meta::VERSION, vec![9]).unwrap();
    assert_eq!(after.version(), Some(1));
    assert!(table.ensure_layout().is_ok(), "the stamp still matches");
}

#[test]
fn a_store_stamped_with_another_shape_is_refused() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());
    meta::write(&store, &header(), &TableMeta::stamped(2)).unwrap();

    let err = table.ensure_layout().unwrap_err();
    assert_eq!(err, Error::LayoutMismatch { stored: Some(2), current: 1 });
    let text = err.to_string();
    assert!(text.contains("stamped Some(2)"), "{text}");
    assert!(text.contains("delete it"), "{text}");
}

#[test]
fn a_meta_row_that_names_no_version_is_refused() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());
    let mut foreign = TableMeta::new();
    foreign.set("whoever", b"not okm".to_vec());
    meta::write(&store, &header(), &foreign).unwrap();

    let err = table.ensure_layout().unwrap_err();
    assert_eq!(err, Error::LayoutMismatch { stored: None, current: 1 });
}

#[test]
fn row_scans_never_surface_the_meta_entry() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());
    table.ensure_layout().unwrap();
    table.put(&MetaKey { id: 7 }, &MetaRow { label: "row".into() }).unwrap();

    assert_eq!(table.scan_keys().unwrap(), vec![MetaKey { id: 7 }]);
    assert_eq!(table.scan_documents_raw().unwrap().len(), 1);
}

#[test]
fn the_meta_payload_survives_a_hand_written_row() {
    // A row written by hand (not through `set_meta`) decodes name-for-name:
    // the format is the contract, not the writer.
    let meta = TableMeta::decode(&{
        let mut encoded = TableMeta::new();
        encoded.set("alpha", vec![1, 2]);
        encoded.set("beta", Vec::new());
        encoded.encode()
    });
    assert_eq!(meta.get("alpha"), Some(&[1u8, 2][..]));
    assert_eq!(meta.get("beta"), Some(&[][..]), "an empty value is a value");
    assert_eq!(meta.version(), None, "no version name in this row");
}