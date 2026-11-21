//! Attach (ADR-0074): `Collection::open` is the one way in, and the store's meta row says
//! what the table is — layout, faces, name, the declaration as bytes, and the capture
//! opt-out — or the open is refused.
//!
//! Locked here: the row a fresh attach writes, the idempotence of a second open, the
//! refusal of a store stamped by another declaration (and that the message names where they
//! differ instead of saying "the bytes differ"), and the capture opt-out travelling as data
//! the log can read rather than as an assembly-time call it has to be told about.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::{ChangelogConfig, ChangelogStore, table_dir};
use okm_core::testing::TestStore;
use okm_core::{Document, DocumentEncode, Error, KeyEncode, meta};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct AttachKey {
    pub id: u32,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(AttachKey)]
#[ok_ns(0x2D)]
#[ok_name("attach_rows")]
pub struct AttachRow {
    pub level: u32,
    pub label: String,
}

/// Same ns, same declared name, same layout — a different shape. This is the store-side
/// twin: the only thing that can disagree is the declaration. It lives in its own module
/// because the derive's ns-uniqueness check is per module: two declarations sharing a
/// namespace in one file is refused, which is what makes two tables fight over a row by
/// accident impossible and by intent explicit.
mod twin {
    use super::AttachKey;
    use okm_core::DocumentEncode;

    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(AttachKey)]
    #[ok_ns(0x2D)]
    #[ok_name("attach_rows")]
    pub struct AttachTwin {
        pub level: u64,
        pub label: u32,
    }
}

pub use twin::AttachTwin;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct QuietKey {
    pub id: u32,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(QuietKey)]
#[ok_ns(0x2E)]
#[ok_name("quiet_rows")]
#[ok_uncaptured]
pub struct QuietRow {
    pub label: String,
}

/// The same declaration, captured. It opens the store `QuietRow` stamped — its own module
/// for the same reason as `twin`: one ns per module is a compile-time rule here.
mod loud {
    use super::QuietKey;
    use okm_core::DocumentEncode;

    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(QuietKey)]
    #[ok_ns(0x2E)]
    #[ok_name("quiet_rows")]
    pub struct LoudRow {
        pub label: String,
    }
}

pub use loud::LoudRow;

fn header<R: Document>() -> Vec<u8> {
    meta::document_header::<R>()
}

#[test]
fn an_open_writes_the_whole_declaration_into_the_meta_row() {
    let store = TestStore::default();
    let _table = okm_core::Collection::<_, AttachKey, AttachRow>::open(store.clone()).unwrap();

    let row = meta::read(&store, &header::<AttachRow>())
        .unwrap()
        .expect("attach writes a row");
    assert_eq!(row.layout_version(), Some(AttachRow::LAYOUT_VERSION));
    assert_eq!(row.faces(), Some(AttachRow::FACES));
    assert_eq!(row.table_name(), Some("attach_rows"));
    let schema = row
        .get(meta::SCHEMA)
        .expect("the declaration itself is in the row");
    let decoded = okm_core::model::schema::CollectionSchema::decode(schema);
    assert_eq!(
        decoded,
        okm_core::model::schema::CollectionSchema::of::<AttachKey, AttachRow>()
    );
    assert_eq!(
        row.get(meta::CAPTURE),
        None,
        "a captured table records nothing: absence is the default"
    );

    // A second open is the ordinary case (every process start) and must not rewrite or fail.
    okm_core::Collection::<_, AttachKey, AttachRow>::open(store.clone()).unwrap();
}

#[test]
fn another_declaration_is_refused_and_the_message_names_where() {
    let store = TestStore::default();
    okm_core::Collection::<_, AttachKey, AttachRow>::open(store.clone()).unwrap();

    let refused = okm_core::Collection::<_, AttachKey, AttachTwin>::open(store);
    match refused {
        Err(Error::SchemaMismatch { what }) => {
            assert!(
                what.contains("key_fields")
                    || what.contains("hot_fields")
                    || what.contains("cold_fields"),
                "the refusal has to name the field that differs, not just say the bytes differ: {what}"
            );
            assert!(
                what.contains("stored") && what.contains("build"),
                "and both sides: {what}"
            );
        }
        Ok(_) => panic!("a store stamped by another declaration must be refused"),
        Err(other) => panic!("wrong refusal: {other}"),
    }
}

#[test]
fn the_capture_opt_out_travels_as_data() {
    let store = TestStore::default();
    okm_core::Collection::<_, QuietKey, QuietRow>::open(store.clone()).unwrap();

    let row = meta::read(&store, &header::<QuietRow>())
        .unwrap()
        .expect("attach writes a row");
    assert_eq!(
        row.get(meta::CAPTURE),
        Some(&b"0"[..]),
        "an opt-out is a fact in the store, not an assembly-time argument"
    );

    // The same shape, declared captured: the schema agrees, the capture decision does not,
    // and that disagreement is the one ADR-0074 exists to make impossible to skip.
    match okm_core::Collection::<_, QuietKey, LoudRow>::open(store) {
        Err(Error::SchemaMismatch { what }) => {
            assert!(what.contains("capture"), "named: {what}");
            assert!(what.contains("off"), "the store's side: {what}");
            assert!(what.contains("on"), "this build's side: {what}");
        }
        Ok(_) => panic!("a store that opted out must not be opened by a capturing declaration"),
        Err(other) => panic!("wrong refusal: {other}"),
    }
}

#[test]
fn attaching_is_what_tells_the_log_to_capture() {
    // ADR-0074 §3: no `Collection::captured`, no `register`. The table opens; the store reads
    // the row attach just wrote; the writes that follow are captured.
    let logs = tempfile::tempdir().unwrap();
    let store = ChangelogStore::new(TestStore::default(), ChangelogConfig::new(logs.path()));
    {
        let app = okm_core::Collection::<_, AttachKey, AttachRow>::open(store).unwrap();
        app.put(
            &AttachKey { id: 1 },
            &AttachRow {
                level: 7,
                label: "captured".to_string(),
            },
        )
        .unwrap();
    }
    let dir = table_dir(logs.path(), &header::<AttachRow>());
    let segments: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".segment"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !segments.is_empty(),
        "opening a table is the whole instruction to capture it: {segments:?}"
    );

    // And the declaration that opted out is honoured without anyone saying so.
    let quiet_logs = tempfile::tempdir().unwrap();
    let quiet_store = ChangelogStore::new(
        TestStore::default(),
        ChangelogConfig::new(quiet_logs.path()),
    );
    {
        let app = okm_core::Collection::<_, QuietKey, QuietRow>::open(quiet_store).unwrap();
        app.put(
            &QuietKey { id: 1 },
            &QuietRow {
                label: "not captured".to_string(),
            },
        )
        .unwrap();
    }
    let quiet_dir = table_dir(quiet_logs.path(), &header::<QuietRow>());
    let wrote = std::fs::read_dir(&quiet_dir)
        .map(|d| {
            d.filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().ends_with(".segment"))
        })
        .unwrap_or(false);
    assert!(!wrote, "an opted-out declaration captured something");
}
