//! Generation identity in a changelog directory that outlives every build (ADR-0056 §5/§6).
//!
//! The path never changes: one directory per table, appended to by one build after another.
//! So a directory holds several generations, each sealed segment says which generation it
//! was written under, and each generation keeps its own `schema-v<N>.json` beside it. What
//! a generation identifies is BYTES — never a directory, and never a build.
//!
//! These tests are the contract in that direction, replacing the earlier assumption that a
//! directory holds exactly one generation (which made a new build refuse to open the log it
//! was supposed to continue).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::changelog::window::{WindowConfig, load_sidecar, read_window, sidecar_of};
use okm_core::changelog::{ChangelogConfig, ChangelogStore, apply_log_in_generation, table_dir};
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

/// Generation 1 — the build that wrote first.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(97)]
#[ok_name("generation_rows")]
pub struct Gen1 {
    pub level: u32,
}

/// Generation 2 — the same table declared afresh. Its bytes are not generation 1's, and
/// that is a fact about segments, not about the directory they sit in.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(97)]
#[ok_layout(version = 2)]
#[ok_name("generation_rows")]
pub struct Gen2 {
    pub level: u32,
}

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(root))
}

fn dir(root: &std::path::Path) -> std::path::PathBuf {
    table_dir(root, <Gen1 as Document>::NS_PREFIX)
}

/// Write one row through `Gen1` and seal it, so the directory carries a real generation 1
/// segment and its declaration.
fn write_generation_one(root: &std::path::Path) {
    let c: Collection<_, K, Gen1> = Collection::captured(store(root)).expect("assemble");
    c.put(&K { id: 1 }, &Gen1 { level: 7 }).expect("put");
}

/// A window is one VOCABULARY as well as one generation. Two segments whose field ids mean
/// different names cannot be described by one schema — a column holds a label, and the same
/// id would have to answer two of them — so the later segment starts the next window. The
/// labels come from a store's intern table, and a store that starts fresh (a test, a rebuild,
/// a restored snapshot) numbers from the beginning again.
#[test]
fn a_window_stops_at_a_vocabulary_change() {
    let log = tempfile::tempdir().expect("root");

    // Window one: field id 1 is `note`, written under one store.
    {
        let c: Collection<_, K, Gen1> = Collection::captured(store(log.path())).expect("assemble");
        c.put(&K { id: 1 }, &Gen1 { level: 7 }).expect("put");
        let mut open = std::collections::BTreeMap::new();
        open.insert(
            "note".to_string(),
            okm_value::DynamicValue::Str("first".to_string()),
        );
        c.merge(&K { id: 1 }, &open).expect("an open field");
    }

    // Window two: a fresh store numbers from 1 again, and 1 is now `tag`.
    {
        let c: Collection<_, K, Gen1> = Collection::captured(store(log.path())).expect("assemble");
        c.put(&K { id: 1 }, &Gen1 { level: 8 }).expect("put");
        let mut open = std::collections::BTreeMap::new();
        open.insert(
            "tag".to_string(),
            okm_value::DynamicValue::Str("second".to_string()),
        );
        c.merge(&K { id: 1 }, &open).expect("an open field");
    }

    let d = dir(log.path());
    let first = read_window(&d, &WindowConfig::default())
        .expect("read")
        .expect("a window");
    assert_eq!(first.segments.len(), 1, "the second segment is not in it");
    assert_eq!(first.dynamic.len(), 1);
    assert_eq!(
        first.dynamic[0].1.keys().cloned().collect::<Vec<_>>(),
        vec!["note".to_string()],
        "the first window's own vocabulary"
    );

    let next = read_window(
        &d,
        &WindowConfig {
            after: Some(first.segments[0].clone()),
            ..WindowConfig::default()
        },
    )
    .expect("read")
    .expect("a second window");
    assert_eq!(next.dynamic.len(), 1);
    assert_eq!(
        next.dynamic[0].1.keys().cloned().collect::<Vec<_>>(),
        vec!["tag".to_string()],
        "and the next window names its own field instead of borrowing the first one's"
    );
}

/// The same directory, the next build: nothing refuses, and the row lands above the last
/// generation 1 segment.
#[test]
fn a_new_generation_registers_and_appends_to_the_same_directory() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());

    let c: Collection<_, K, Gen2> = Collection::captured(store(log.path()))
        .expect("a directory written by another generation is exactly what an upgrade opens");
    c.put(&K { id: 2 }, &Gen2 { level: 8 }).expect("append");
    // Sealing is what a clean stop does; nothing else would put the second segment on disk
    // under a number a reader can see.
    drop(c);

    let mut stamped: Vec<u8> = std::fs::read_dir(dir(log.path()))
        .expect("dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "segment")
        })
        .map(|p| {
            okm_core::changelog::segment_info(&p)
                .expect("info")
                .meta
                .layout_version
        })
        .collect();
    stamped.sort_unstable();
    assert_eq!(
        stamped,
        vec![1, 2],
        "both generations, side by side, one directory"
    );
}

#[test]
fn every_generation_keeps_its_own_declaration() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());
    let _: Collection<_, K, Gen2> = Collection::captured(store(log.path())).expect("assemble");

    let d = dir(log.path());
    assert_eq!(sidecar_of(&d, 1).expect("generation 1").layout_version, 1);
    assert_eq!(sidecar_of(&d, 2).expect("generation 2").layout_version, 2);
    // "The newest declaration" is a different question from "this segment's declaration",
    // and it is the one a store being built today is declared with.
    assert_eq!(
        load_sidecar(&d).expect("newest").layout_version,
        2,
        "newest, by number — not by file name"
    );
}

#[test]
fn a_generation_without_a_sidecar_is_refused_by_number() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());
    let _: Collection<_, K, Gen2> = Collection::captured(store(log.path())).expect("assemble");
    let d = dir(log.path());
    std::fs::remove_file(d.join(okm_core::changelog::sidecar_name(2))).expect("drop it");

    let err = sidecar_of(&d, 2).expect_err("no declaration for those bytes");
    let text = err.to_string();
    assert!(text.contains('2'), "the generation is named: {text}");
    assert!(
        text.contains("schema-v2.json"),
        "and so is the file that is missing: {text}"
    );
}

/// A window is one generation: its declaration is what a sink or a replay decodes with, so
/// a segment of another generation ends the window instead of joining it.
#[test]
fn a_window_stops_at_a_generation_change() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());
    let second: Collection<_, K, Gen2> = Collection::captured(store(log.path())).expect("assemble");
    second
        .put(&K { id: 2 }, &Gen2 { level: 8 })
        .expect("append");
    drop(second);

    let d = dir(log.path());
    let first = read_window(&d, &WindowConfig::default())
        .expect("read")
        .expect("a window");
    assert_eq!(first.layout_version, 1, "the window's own generation");
    assert_eq!(first.segments.len(), 1, "generation 2 is not in it");

    let next = read_window(
        &d,
        &WindowConfig {
            max_segments: 8,
            after: first.segments.last().cloned(),
        },
    )
    .expect("read")
    .expect("a second window");
    assert_eq!(next.layout_version, 2);
    assert_eq!(next.rows.len(), 1, "the row generation 2 appended");
}

/// The byte copy is only honest for bytes this build's declaration produced: another
/// generation's payload is not a row under this store's shape, so it is refused and named.
#[test]
fn a_byte_copy_refuses_a_segment_of_another_generation() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());

    let target = TestStore::slatedb_mem();
    let err = apply_log_in_generation(log.path(), <Gen1 as Document>::NS_PREFIX, &target, None, 2)
        .expect_err("generation 1 bytes under a generation 2 store");
    let text = err.to_string();
    assert!(text.contains("layout 1") && text.contains('2'), "{text}");
    assert!(
        text.contains("ADR-0056"),
        "the rule the refusal follows is named: {text}"
    );
}
