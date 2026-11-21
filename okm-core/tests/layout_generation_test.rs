//! Generation identity in a changelog that outlives every build (ADR-0056 §5/§6, ADR-0075).
//!
//! The generation is one bit of the address, so a bump moves the table to the other range and
//! every segment of that generation follows it: one directory per ADDRESS, not one per table
//! identity, and each generation keeps its own `schema-v<N>.json` beside its own segments.
//! What a generation identifies is BYTES — never a directory, and never a build.
//!
//! A version that does not flip the parity is refused rather than served: it would put two
//! generations at one address, and the store's own row would answer for the wrong one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::window::{WindowConfig, declaration_of, load_declaration, read_window};
use okm_core::changelog::{ChangelogConfig, ChangelogStore, apply_log_in_generation, table_dir};
use okm_core::testing::TestStore;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};
use okm_value::{Dict, Variant};

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
#[ok_shared_ns_in_tests]
#[ok_ref(K)]
#[ok_ns(97)]
#[ok_layout(version = 2)]
#[ok_name("generation_rows")]
pub struct Gen2 {
    pub level: u32,
}

/// Generation 3 — a bump that lands on the SAME parity as generation 1. The address cannot
/// tell them apart, so the store refuses instead of serving one as the other (ADR-0075 §2:
/// the version has to change the parity, which means it has to move by one).
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_shared_ns_in_tests]
#[ok_ref(K)]
#[ok_ns(97)]
#[ok_layout(version = 3)]
#[ok_name("generation_rows")]
pub struct Gen3 {
    pub level: u32,
}

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(TestStore::default(), ChangelogConfig::new(root))
}

/// Generation 1's directory — the address, bit included.
fn dir(root: &std::path::Path) -> std::path::PathBuf {
    table_dir(root, <Gen1 as Document>::NS_PREFIX)
}

/// Generation 2's directory: one parity apart, so one bit apart in the path.
fn dir_two(root: &std::path::Path) -> std::path::PathBuf {
    table_dir(root, <Gen2 as Document>::NS_PREFIX)
}

/// Every sealed segment's `layout_version` in one directory.
fn stamped(root: &std::path::Path) -> Vec<u8> {
    let mut out: Vec<u8> = std::fs::read_dir(root)
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
    out.sort_unstable();
    out
}

/// Write one row through `Gen1` and seal it, so the directory carries a real generation 1
/// segment and its declaration.
fn write_generation_one(root: &std::path::Path) {
    let c: Collection<_, K, Gen1> = Collection::open(store(root)).expect("assemble");
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
        let c: Collection<_, K, Gen1> = Collection::open(store(log.path())).expect("assemble");
        c.put(&K { id: 1 }, &Gen1 { level: 7 }).expect("put");
        let mut open = Dict::new();
        open.insert(
            "note".to_string(),
            okm_value::Variant::Str("first".to_string()),
        );
        c.put(&K { id: 1 }, &Variant::Map(open))
            .expect("an open field");
    }

    // Window two: a fresh store numbers from 1 again, and 1 is now `tag`.
    {
        let c: Collection<_, K, Gen1> = Collection::open(store(log.path())).expect("assemble");
        c.put(&K { id: 1 }, &Gen1 { level: 8 }).expect("put");
        let mut open = Dict::new();
        open.insert(
            "tag".to_string(),
            okm_value::Variant::Str("second".to_string()),
        );
        c.put(&K { id: 1 }, &Variant::Map(open))
            .expect("an open field");
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

/// The next build: nothing refuses, and its row lands in the OTHER range — the generation is
/// a bit of the address, so the directory moves with it and generation 1's log is left whole.
#[test]
fn a_new_generation_appends_in_its_own_directory() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());

    let c: Collection<_, K, Gen2> = Collection::open(store(log.path()))
        .expect("a store written by the generation before this one is what an upgrade opens");
    c.put(&K { id: 2 }, &Gen2 { level: 8 }).expect("append");
    // Sealing is what a clean stop does; nothing else would put the second segment on disk
    // under a number a reader can see.
    drop(c);

    assert_ne!(
        dir(log.path()),
        dir_two(log.path()),
        "one bit of the address, so one directory per generation (ADR-0075 §2)"
    );
    assert_eq!(
        stamped(&dir(log.path())),
        vec![1],
        "generation 1's directory still holds only generation 1"
    );
    assert_eq!(
        stamped(&dir_two(log.path())),
        vec![2],
        "generation 2 appended beside its own declaration, not beside generation 1's"
    );
}

#[test]
fn every_generation_keeps_its_own_declaration() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());
    {
        // A declaration is a log record (ADR-0074 §2), so generation 2's exists once
        // generation 2 has written — not because the directory was touched.
        let g2: Collection<_, K, Gen2> = Collection::open(store(log.path())).expect("assemble");
        g2.put(&K { id: 2 }, &Gen2 { level: 8 }).expect("append");
    }

    // Each directory answers for its own generation, and only for it: a number the segments
    // there do not declare is not a guess to make.
    let one = dir(log.path());
    let two = dir_two(log.path());
    assert_eq!(
        declaration_of(&one, 1)
            .expect("generation 1")
            .layout_version,
        1
    );
    assert_eq!(
        declaration_of(&two, 2)
            .expect("generation 2")
            .layout_version,
        2
    );
    assert!(
        declaration_of(&two, 1).is_err(),
        "generation 2's directory declares no layout 1"
    );
    // "The newest declaration in a directory" is still the one a store being built today is
    // declared with.
    assert_eq!(
        load_declaration(&two).expect("newest").layout_version,
        2,
        "newest, by number — not by file name"
    );
}

#[test]
fn a_generation_no_segment_declares_is_refused_by_number() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());
    let _: Collection<_, K, Gen2> = Collection::open(store(log.path())).expect("assemble");
    let d = dir(log.path());

    // A declaration is whatever a SEGMENT carries (ADR-0074 §2). A number no segment in the
    // directory declares is not a guess to make: it is a stop that names the number.
    let err = declaration_of(&d, 7).expect_err("no segment declares layout 7");
    let text = err.to_string();
    assert!(text.contains('7'), "the generation is named: {text}");
}

/// A window is one generation: its declaration is what a sink or a replay decodes with, and
/// because a bump moves the directory there is nothing to stop at — the other generation's
/// segments are simply not in this path.
#[test]
fn each_generation_reads_its_own_window() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());
    let second: Collection<_, K, Gen2> = Collection::open(store(log.path())).expect("assemble");
    second
        .put(&K { id: 2 }, &Gen2 { level: 8 })
        .expect("append");
    drop(second);

    let first = read_window(&dir(log.path()), &WindowConfig::default())
        .expect("read")
        .expect("a window");
    assert_eq!(first.layout_version, 1, "the window's own generation");
    assert_eq!(first.segments.len(), 1, "generation 2 is not in this path");

    let second = read_window(&dir_two(log.path()), &WindowConfig::default())
        .expect("read")
        .expect("its own window");
    assert_eq!(second.layout_version, 2);
    assert_eq!(second.rows.len(), 1, "the row generation 2 appended");
}

/// A bump that does not flip the parity puts two generations at one address, and the store's
/// row at that address answers for the older one — so attach refuses instead of serving one
/// generation as the other. This is why a version may only move by one (ADR-0075 §2).
#[test]
fn a_bump_that_does_not_flip_the_parity_is_refused() {
    // One ENGINE, two declarations: the row generation 1 wrote is the row generation 3 would
    // find at its own address, because both versions are odd.
    let engine = TestStore::default();
    let _: Collection<_, K, Gen1> = Collection::open(engine.clone()).expect("generation 1");

    let err = match Collection::<_, K, Gen3>::open(engine) {
        Ok(_) => panic!("generation 3 sits where generation 1 does"),
        Err(err) => err,
    };
    let text = err.to_string();
    assert!(
        text.contains("v1") && text.contains("v3"),
        "the store's generation and this one are both named: {text}"
    );
}

/// The byte copy is only honest for bytes this build's declaration produced: another
/// generation's payload is not a row under this store's shape, so it is refused and named.
#[test]
fn a_byte_copy_refuses_a_segment_of_another_generation() {
    let log = tempfile::tempdir().expect("root");
    write_generation_one(log.path());

    let target = TestStore::default();
    let err = apply_log_in_generation(log.path(), <Gen1 as Document>::NS_PREFIX, &target, None, 2)
        .expect_err("generation 1 bytes under a generation 2 store");
    let text = err.to_string();
    assert!(text.contains("layout 1") && text.contains('2'), "{text}");
    assert!(
        text.contains("ADR-0056"),
        "the rule the refusal follows is named: {text}"
    );
}
