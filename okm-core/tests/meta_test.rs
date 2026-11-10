//! Table meta (ADR-0043): fixed face 0x0F — ONE name → bytes entry per table,
//! describing the store instead of holding its data.
//!
//! The row carries what the CALLER names plus the declaration's face fingerprint
//! (ADR-0048). It carries no layout number: the payload's generation is the segment
//! header's statement (ADR-0042 as amended), because a reader that must decode a row
//! before trusting it cannot ask that row which format wrote it.
//!
//! Locks: the address (highest fixed face, no suffix), the fingerprint a fresh store
//! gets, the locked bytes of a caller-named row, the form once two names exist, and
//! the entry's exclusion from row/document scans.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::storage::VirtualStorage;
use okm_core::{Document, DocumentEncode, KeyEncode, TableMeta, meta};

use okm_async::TestStore;
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
    assert_eq!(
        stamped.faces(),
        Some(MetaRow::FACES),
        "the fingerprint is the declaration's own, computed, never typed"
    );

    let key = meta::meta_key(&header());
    assert_eq!(key, vec![0x00, 0x5A, 0x0F], "`[ns 2B][0x0F]`, no suffix");

    // [count u16 BE] [name_len varint] [name] [value_len varint] [value]
    let payload = store.get(&key).unwrap().unwrap();
    let stamped_row = TableMeta::decode(&payload);
    assert_eq!(stamped_row.faces(), Some(MetaRow::FACES));
    assert_eq!(
        stamped_row.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        vec!["faces"],
        "one name, the runtime's own, and nothing else"
    );

    // A row an app wrote itself — one caller name, no fingerprint — parses, and its
    // bytes are locked so nothing grows into that shape silently.
    let app_row = TestStore::default();
    let mut meta_row = TableMeta::new();
    meta_row.set("build", b"1".to_vec());
    meta::write(&app_row, &header(), &meta_row).unwrap();
    assert_eq!(
        app_row.get(&key).unwrap().unwrap(),
        vec![0x00, 0x01, 0x05, b'b', b'u', b'i', b'l', b'd', 0x01, b'1'],
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

    // The fingerprint is the first-touch verb's write, on a store that has no row yet.
    let stamped = table.ensure_layout().unwrap();
    assert_eq!(stamped.faces(), Some(MetaRow::FACES));

    // The caller's name rides beside it, in name order, and the row decodes into the
    // same map (round trip through the stored bytes).
    let after = table.set_meta("build", b"20261028".to_vec()).unwrap();
    assert_eq!(after.get("build"), Some(&b"20261028"[..]));
    assert_eq!(
        after.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        vec!["build", "faces"],
        "the caller's map plus the runtime's one name"
    );
    let stored = table.meta().unwrap().unwrap();
    assert_eq!(stored, after);

    // Rewriting a caller name re-reads the row it finds, so the fingerprint survives.
    let after = table.set_meta("build", b"20261030".to_vec()).unwrap();
    assert_eq!(after.faces(), Some(MetaRow::FACES));
}

#[test]
fn a_row_with_no_fingerprint_is_untrusted_not_refused() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());
    let mut foreign = TableMeta::new();
    foreign.set("whoever", b"not okm".to_vec());
    meta::write(&store, &header(), &foreign).unwrap();

    // Opening is fine: this row does not say which generation wrote the bytes, and it
    // is not the place to ask (ADR-0042 — the segment header is). What it does say is
    // that no fingerprint was ever recorded, so the derived faces are untrusted
    // rather than wrong.
    assert!(table.ensure_layout().is_ok());
    assert_eq!(
        table.faces_status().unwrap(),
        okm_core::FaceStatus::Unknown {
            current: MetaRow::FACES
        }
    );
}

#[test]
fn row_scans_never_surface_the_meta_entry() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());
    table.ensure_layout().unwrap();
    table
        .put(
            &MetaKey { id: 7 },
            &MetaRow {
                label: "row".into(),
            },
        )
        .unwrap();

    assert_eq!(
        table
            .scan_keys(&okm_core::PrefixProbe::empty())
            .map(|r| r.unwrap())
            .collect::<Vec<_>>(),
        vec![MetaKey { id: 7 }]
    );
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
    assert_eq!(meta.faces(), None, "no fingerprint name in this row");
}