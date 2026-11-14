//! Table meta (ADR-0043): fixed face 0x1F — ONE name → bytes entry per table,
//! describing the store instead of holding its data.
//!
//! Three names are the runtime's: `version` (the layout generation the writer
//! speaks, `Document::LAYOUT_VERSION`), `faces` (the declaration's face
//! fingerprint, ADR-0048) and `table` (its `#[ok_name]`). The version is the
//! load-bearing one: a layout change is a cutover, so a store stamped with a
//! number this build does not speak is REFUSED at attach — and so is a row that
//! records no number at all, because a row this build did not stamp is not a store
//! of this layout. The only exempt store is one with no row yet: that is fresh,
//! and the first-touch verb seeds it.
//!
//! Locks: the address (highest fixed face, no suffix), the three names a fresh
//! store gets, the fingerprint's value, the locked bytes of a caller-named row,
//! the form once a caller name exists, the refusal shapes (another generation's
//! number, no number at all, a taken table name), and the entry's exclusion from
//! row/document scans.

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
    assert_eq!(
        stamped.layout_version(),
        Some(<MetaRow as Document>::LAYOUT_VERSION),
        "the store records the layout generation that wrote it"
    );

    let key = meta::meta_key(&header());
    assert_eq!(key, vec![0x00, 0x5A, 0x1F], "`[ns 2B][0x1F]`, no suffix");

    // [count u16 BE] [name_len varint] [name] [value_len varint] [value]
    let payload = store.get(&key).unwrap().unwrap();
    let stamped_row = TableMeta::decode(&payload);
    assert_eq!(stamped_row.faces(), Some(MetaRow::FACES));
    assert_eq!(
        stamped_row.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        vec!["faces", "table", "version"],
        "the runtime's fingerprint, the table's own name and the layout it speaks — nothing else"
    );
    assert_eq!(
        stamped_row.table_name(),
        Some("meta_probe"),
        "the name is what makes one ns answerable to one table"
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
        vec!["build", "faces", "table", "version"],
        "the caller's map plus the three names the runtime owns"
    );
    let stored = table.meta().unwrap().unwrap();
    assert_eq!(stored, after);

    // Rewriting a caller name re-reads the row it finds, so the fingerprint survives.
    let after = table.set_meta("build", b"20261030".to_vec()).unwrap();
    assert_eq!(after.faces(), Some(MetaRow::FACES));

    // And the layout name is the runtime's: a caller writing `version` itself gets
    // this build's number back, not its own bytes.
    let after = table.set_meta("version", vec![9]).unwrap();
    assert_eq!(
        after.layout_version(),
        Some(<MetaRow as Document>::LAYOUT_VERSION),
        "the shape stamp cannot be edited away by a caller"
    );
}

#[test]
fn a_store_stamped_with_another_layout_is_refused() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());

    // A row this build did NOT write: another layout's number, no fingerprint.
    let mut foreign = TableMeta::new();
    foreign.set(meta::VERSION, vec![9]);
    meta::write(&store, &header(), &foreign).unwrap();

    match table.ensure_layout() {
        Err(okm_core::Error::LayoutMismatch { stored, current }) => {
            assert_eq!(stored, 9, "the number the store records");
            assert_eq!(
                current,
                <MetaRow as Document>::LAYOUT_VERSION,
                "and the one this build speaks"
            );
        }
        other => panic!("expected LayoutMismatch, got {other:?}"),
    }

    // Writing through this handle is refused too: relabelling a store this build
    // cannot read would make it claim a layout it never had.
    assert!(matches!(
        table.set_meta("build", b"1".to_vec()),
        Err(okm_core::Error::LayoutMismatch { stored: 9, .. })
    ));
    assert_eq!(
        table.meta().unwrap().unwrap().layout_version(),
        Some(9),
        "the refused writes left the store's own number untouched"
    );
}

#[test]
fn a_row_that_records_no_layout_name_is_refused() {
    let store = TestStore::default();
    let table = <MetaRow as Document>::collection(store.clone());

    // A row this build did not write: a caller's name and a fingerprint, no
    // generation. There is no older layout to tolerate — a row this build did not
    // stamp describes bytes no generation vouches for.
    let mut foreign = TableMeta::new();
    foreign.set(meta::FACES, MetaRow::FACES.to_vec());
    foreign.set("whoever", b"not okm".to_vec());
    meta::write(&store, &header(), &foreign).unwrap();

    match table.ensure_layout() {
        Err(okm_core::Error::LayoutUnstamped { current }) => assert_eq!(
            current,
            <MetaRow as Document>::LAYOUT_VERSION,
            "the one number this build speaks"
        ),
        other => panic!("expected LayoutUnstamped, got {other:?}"),
    }

    // Writing through this handle is refused too: stamping the row would label a
    // store whose rows nothing vouches for.
    assert!(matches!(
        table.set_meta("build", b"1".to_vec()),
        Err(okm_core::Error::LayoutUnstamped { .. })
    ));
    assert_eq!(
        table.meta().unwrap().unwrap().layout_version(),
        None,
        "the refused writes left the row exactly as they found it"
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
    assert_eq!(meta.layout_version(), None, "and no layout name either");
}

/// ns 92: two tables that share a namespace. The compile-time claim stops at one
/// module and `#[ok_shared_ns]` silences it on purpose, so this is exactly the
/// case only the STORE can refuse — the two declarations would otherwise share
/// every face, cursor and this meta row.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MetaKey)]
#[ok_ns(92)]
#[ok_name("alpha")]
pub struct Alpha {
    pub label: String,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MetaKey)]
#[ok_ns(92)]
#[ok_shared_ns]
#[ok_name("beta")]
pub struct Beta {
    pub label: String,
}

/// ns 93, unnamed: `#[ok_name]` absent means nothing to claim and nothing to check.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MetaKey)]
#[ok_ns(93)]
pub struct Nameless {
    pub label: String,
}

#[test]
fn two_tables_in_one_ns_cannot_both_attach() {
    let store = TestStore::default();
    let alpha = <Alpha as Document>::collection(store.clone());
    let beta = <Beta as Document>::collection(store.clone());

    alpha.ensure_layout().unwrap();
    match beta.ensure_layout() {
        Err(okm_core::Error::TableNameTaken {
            ns,
            declared,
            existing,
        }) => {
            assert_eq!(ns, 92);
            assert_eq!(declared, "beta");
            assert_eq!(existing, "alpha");
        }
        other => panic!("expected TableNameTaken, got {other:?}"),
    }

    // The refused declaration left the row alone: the first table still owns the ns.
    let stored = alpha.meta().unwrap().unwrap();
    assert_eq!(stored.table_name(), Some("alpha"));
    assert_eq!(stored.faces(), Some(Alpha::FACES));
}

#[test]
fn the_same_table_attaches_twice() {
    let store = TestStore::default();
    <Alpha as Document>::collection(store.clone())
        .ensure_layout()
        .unwrap();
    let second = <Alpha as Document>::collection(store.clone())
        .ensure_layout()
        .unwrap();
    assert_eq!(second.table_name(), Some("alpha"));
    assert_eq!(second.faces(), Some(Alpha::FACES));
}

#[test]
fn an_unnamed_table_records_no_name() {
    let store = TestStore::default();
    let meta = <Nameless as Document>::collection(store.clone())
        .ensure_layout()
        .unwrap();
    assert_eq!(meta.table_name(), None, "no name, no claim");
    assert_eq!(meta.faces(), Some(Nameless::FACES));
}
