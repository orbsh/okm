//! A pin does not wait for the drainer (ADR-0071 §2).
//!
//! `compact_against` refuses when the lake does not reach the newest sealed segment, and that is
//! correct — a pointer covers what it names. What it cannot do is make the lake catch up, so a
//! cutover would wait on the drainer's schedule. `compact_against_exporting` runs that export
//! itself, through the same writer and the same consumer position, and then points at the
//! version it produced.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::changelog::{ChangelogConfig, ChangelogStore, lake_baseline, last_sealed, table_dir};
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};
use okm_lake::baseline::{compact_against, compact_against_exporting, verify_at};
use okm_lake::{DrainConfig, drain};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(74)]
#[ok_name("export_rows")]
pub struct R {
    pub level: u32,
    pub name: String,
}

fn collection(root: &std::path::Path) -> Collection<ChangelogStore<TestStore>, K, R> {
    Collection::open(ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root),
    ))
    .expect("the log opens")
}

fn fill(root: &std::path::Path, ids: std::ops::Range<u64>) {
    let app = collection(root);
    for id in ids {
        app.put(
            &K { id },
            &R {
                level: 1,
                name: format!("row {id}"),
            },
        )
        .expect("put");
    }
    drop(app);
}

fn lake_table(lake: &std::path::Path) -> std::path::PathBuf {
    let mut entries: Vec<_> = std::fs::read_dir(lake)
        .expect("the lake directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.join("_delta_log").is_dir())
        .collect();
    assert_eq!(entries.len(), 1, "one table was drained");
    entries.pop().expect("the table directory")
}

fn drain_into(logs: &std::path::Path, lake: &std::path::Path) {
    let report = drain(logs, &DrainConfig::new(lake, "verify")).expect("drain");
    assert_eq!(report.tables, 1, "one table was drained");
}

fn coverage_of(dir: &std::path::Path) -> u64 {
    last_sealed(dir)
        .expect("a sealed segment")
        .split('.')
        .next()
        .expect("the ordinal prefix")
        .parse()
        .expect("a number")
}

fn segments_left(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .expect("the table directory")
        .filter(|entry| {
            entry
                .as_ref()
                .map(|entry| entry.file_name().to_string_lossy().ends_with(".segment"))
                .unwrap_or(false)
        })
        .count()
}

fn store(logs: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(logs))
}

/// What the plain pin does with a lake that stopped short: it points at the coverage the lake
/// has, and the newer sealed segment stays as the tail. That is the fallback working — and the
/// reason a cutover used to wait for the drainer.
#[test]
fn the_plain_pin_covers_what_the_lake_holds_and_stops_there() {
    let logs = tempfile::tempdir().expect("logs");
    let lake = tempfile::tempdir().expect("lake");
    fill(logs.path(), 0..8);
    drain_into(logs.path(), lake.path());
    let header = <R as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(logs.path(), &header);
    let first = coverage_of(&dir);

    fill(logs.path(), 8..12);
    let behind = coverage_of(&dir);
    assert!(behind > first, "the second run added a segment");

    let compacted = compact_against::<_, R>(&store(logs.path()), &lake_table(lake.path()), 0, 0)
        .expect("a lake that is behind still points at what it holds");
    assert!(compacted.baseline.is_some());
    let written = lake_baseline(logs.path(), &header)
        .expect("read the pointer")
        .expect("a pointer was written");
    assert_eq!(written.covers, first, "it covered what the lake had");
    assert!(
        segments_left(&dir) > 0,
        "the segment the lake does not hold is still the tail"
    );
}

/// The case the export leg exists for: the pin runs the drainer itself, so the pointer covers
/// the newest sealed segment instead of whatever the drainer last managed.
#[test]
fn a_pin_exports_the_tail_it_would_have_waited_for() {
    let logs = tempfile::tempdir().expect("logs");
    let lake = tempfile::tempdir().expect("lake");
    fill(logs.path(), 0..8);
    drain_into(logs.path(), lake.path());
    let header = <R as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(logs.path(), &header);
    let first = coverage_of(&dir);

    fill(logs.path(), 8..12);
    let behind = coverage_of(&dir);
    assert!(behind > first, "the second run added a segment");

    let cfg = DrainConfig::new(lake.path(), "verify");
    let (compacted, report) =
        compact_against_exporting::<_, R>(&store(logs.path()), &cfg, 0).expect("the pin exports");
    assert_eq!(report.rows, 4, "the rows the lake did not have: {report:?}");
    assert!(compacted.baseline.is_some(), "a reference, not a scan");

    let written = lake_baseline(logs.path(), &header)
        .expect("read the pointer")
        .expect("a pointer was written");
    assert_eq!(written.version, 1, "the export wrote a second version");
    assert_eq!(
        written.covers, behind,
        "the coverage is the segment the export just took"
    );
    verify_at(&lake_table(lake.path()), &written).expect("the reference it wrote verifies");
    assert_eq!(
        segments_left(&dir),
        0,
        "everything the lake now holds is gone"
    );
}

/// A lake that is already current is not exported a second time: the drain has nothing to take,
/// and the pointer names the version that was already there.
#[test]
fn a_current_lake_is_not_exported_again() {
    let logs = tempfile::tempdir().expect("logs");
    let lake = tempfile::tempdir().expect("lake");
    fill(logs.path(), 0..8);
    drain_into(logs.path(), lake.path());
    let header = <R as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(logs.path(), &header);
    let covers = coverage_of(&dir);

    let cfg = DrainConfig::new(lake.path(), "verify");
    let (_, report) = compact_against_exporting::<_, R>(&store(logs.path()), &cfg, 0)
        .expect("nothing to export is not a failure");
    assert_eq!(report.rows, 0, "the consumer was already at the end");

    let written = lake_baseline(logs.path(), &header)
        .expect("read the pointer")
        .expect("a pointer was written");
    assert_eq!(written.version, 0, "no new version appeared");
    assert_eq!(written.covers, covers);
}

/// A table with nothing sealed has nothing a pointer could cover, and a lock-free store has no
/// log at all: both refuse before anything is exported or deleted.
#[test]
fn a_table_with_nothing_sealed_is_refused() {
    let logs = tempfile::tempdir().expect("logs");
    let lake = tempfile::tempdir().expect("lake");
    let cfg = DrainConfig::new(lake.path(), "verify");
    let refused = compact_against_exporting::<_, R>(&store(logs.path()), &cfg, 0)
        .expect_err("nothing sealed");
    assert!(refused.contains("nothing sealed"), "{refused}");
}
