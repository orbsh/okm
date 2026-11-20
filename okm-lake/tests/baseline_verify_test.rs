//! A pointer is verified, not trusted (ADR-0071 §4): the lake says whether it still covers
//! the segment the log directory stopped at, and a refusal names what it found instead.
//!
//! The four refusals pinned here are the four ways a pointer can lie: it names a table this is
//! not, a version the lake does not have, a version the lake has moved past, or coverage the
//! live windows do not reach.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::changelog::replay::replay_after;
use okm_core::changelog::{
    ChangelogConfig, ChangelogStore, LakeBaseline, header_hex, lake_baseline, last_sealed,
    table_dir,
};
use okm_core::schema::CollectionSchema;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};
use okm_dynamic::DynamicCollection;
use okm_lake::baseline::{compact_against, verify_at};
use okm_lake::restore::restore_at;
use okm_lake::{DrainConfig, drain};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(73)]
#[ok_name("baseline_rows")]
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

/// Write a run of rows and let the drop seal the tail.
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

/// The lake's table directory — its only one, and named after the table's header in hex.
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
    assert!(report.rows > 0, "rows went to the lake");
}

fn pointer(version: u64, covers: u64) -> LakeBaseline {
    LakeBaseline {
        // The lake is addressed by the header in hex, not by the declared name.
        table: okm_core::changelog::header_hex(<R as Document>::NS_PREFIX),
        version,
        covers,
    }
}

/// The newest sealed segment's ordinal: what a compaction may claim to cover.
fn coverage_of(dir: &std::path::Path) -> u64 {
    last_sealed(dir)
        .expect("a sealed segment")
        .split('.')
        .next()
        .expect("the ordinal prefix")
        .parse()
        .expect("a number")
}

#[test]
fn a_pointer_is_true_only_while_the_lake_still_says_so() {
    let logs = tempfile::tempdir().expect("logs");
    let lake = tempfile::tempdir().expect("lake");
    fill(logs.path(), 0..8);
    let header = <R as Document>::NS_PREFIX.to_vec();
    let covers = coverage_of(&table_dir(logs.path(), &header));
    drain_into(logs.path(), lake.path());
    let table = lake_table(lake.path());

    let truth = pointer(0, covers);
    let coverage = verify_at(&table, &truth).expect("the lake agrees");
    assert_eq!(coverage.version, 0);
    assert_eq!(coverage.covers, covers);
    assert_eq!(coverage.files, 1, "one window so far");

    // 1. A table this is not.
    let other = LakeBaseline {
        table: "00ff".to_string(),
        ..truth.clone()
    };
    let refused = verify_at(&table, &other).expect_err("a foreign pointer");
    assert!(refused.contains("00ff"), "{refused}");

    // 2. A version the lake does not have.
    let ahead = LakeBaseline {
        version: 1,
        ..truth.clone()
    };
    let refused = verify_at(&table, &ahead).expect_err("a version from the future");
    assert!(refused.contains("newest is 0"), "{refused}");

    // 3. Coverage the live windows do not reach.
    let over = LakeBaseline {
        covers: covers + 1,
        ..truth.clone()
    };
    let refused = verify_at(&table, &over).expect_err("coverage it does not have");
    assert!(
        refused.contains(&format!("up to {covers}")),
        "the refusal names what it found: {refused}"
    );

    // 4. A version the lake has moved past still verifies, against ITS OWN state: that is
    //    what makes a pinned version a baseline rather than a moving target. And version 0
    //    cannot borrow the coverage version 1 added.
    fill(logs.path(), 8..12);
    drain_into(logs.path(), lake.path());
    let later = coverage_of(&table_dir(logs.path(), &header));
    assert!(later > covers, "the second window added segments");

    let still = verify_at(&table, &truth).expect("an old version keeps its own state");
    assert_eq!(still.version, 0);
    assert_eq!(
        still.files, 1,
        "version 0 held one window, whatever came after"
    );
    assert_eq!(still.covers, covers);

    let fresh = verify_at(&table, &pointer(1, later)).expect("the newest version verifies");
    assert_eq!(fresh.files, 2, "the second window joined version 1");

    let borrowed =
        verify_at(&table, &pointer(0, later)).expect_err("coverage from a version it is not");
    assert!(
        borrowed.contains(&format!("up to {covers}")),
        "the refusal names the coverage it found: {borrowed}"
    );
}

/// Segments still on disk in a table directory.
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

#[test]
fn a_compaction_points_at_what_the_lake_actually_holds() {
    let logs = tempfile::tempdir().expect("logs");
    let lake = tempfile::tempdir().expect("lake");
    fill(logs.path(), 0..8);
    let header = <R as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(logs.path(), &header);
    let covers = coverage_of(&dir);
    drain_into(logs.path(), lake.path());
    let table = lake_table(lake.path());

    // The store is the same store the application runs: compaction needs the claim, and
    // the coverage it will delete by comes from the lake, not from the log.
    let store = ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(logs.path()));
    let compacted =
        compact_against::<_, R>(&store, &table, 0, 0).expect("compact against the lake");
    assert!(compacted.baseline.is_some(), "a reference, not a scan");
    assert_eq!(segments_left(&dir), 0, "everything the lake holds is gone");

    let written = lake_baseline(logs.path(), &header)
        .expect("read the pointer")
        .expect("a pointer was written");
    assert_eq!(written.version, 0);
    assert_eq!(written.covers, covers, "the coverage came from the lake");
    assert_eq!(written.table, header_hex(&header));
    verify_at(&table, &written).expect("the reference it wrote verifies");

    // A lake that holds nothing refuses, and leaves the log as it was: the caller can still
    // write the scanned baseline instead.
    let elsewhere = tempfile::tempdir().expect("a lake with no log");
    let other = tempfile::tempdir().expect("other logs");
    fill(other.path(), 0..2);
    let other_header = <R as Document>::NS_PREFIX.to_vec();
    let other_dir = table_dir(other.path(), &other_header);
    let store = ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(other.path()));
    compact_against::<_, R>(&store, elsewhere.path(), 0, 0).expect_err("no lake to point at");
    assert!(
        lake_baseline(other.path(), &other_header)
            .expect("read")
            .is_none(),
        "a refused compaction writes no pointer"
    );
    assert!(segments_left(&other_dir) > 0, "and deletes no segment");
}

#[test]
fn a_pointered_table_comes_back_from_the_lake_and_then_the_tail() {
    let logs = tempfile::tempdir().expect("logs");
    let lake = tempfile::tempdir().expect("lake");
    fill(logs.path(), 0..8);
    let header = <R as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(logs.path(), &header);
    let covers = coverage_of(&dir);
    drain_into(logs.path(), lake.path());
    let table = lake_table(lake.path());
    let store = ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(logs.path()));
    compact_against::<_, R>(&store, &table, 0, 0).expect("compact against the lake");

    // The tail: writes the lake does not hold. They must survive the compaction, and they
    // must number themselves ABOVE the reference — a reference is a high-water mark, not a
    // file that happens to be last.
    let app = collection(logs.path());
    for id in 8..12 {
        app.put(
            &K { id },
            &R {
                level: 2,
                name: format!("tail {id}"),
            },
        )
        .expect("put");
    }
    drop(app);

    // A reader with nothing local: the lake holds the state, the log holds the tail.
    let blank = TestStore::slatedb_mem();
    let mut plain: Collection<_, K, R> = Collection::open(blank.shared_handle()).unwrap();
    let restored = restore_at(&dir, &table, &mut plain).expect("restore lake@V");
    assert_eq!(restored.version, 0);
    assert_eq!(restored.covers, covers);
    assert_eq!(restored.rows, 8, "the lake's own rows, not the tail's");

    let target = DynamicCollection::new(
        blank.shared_handle(),
        u16::from_be_bytes([header[0], header[1]]),
        CollectionSchema::of::<K, R>(),
        Vec::new(),
    );
    let tail = replay_after(&dir, restored.covers, &target).expect("replay the tail");
    assert_eq!(tail.puts, 4, "the rows the lake does not have: {tail:?}");
    for id in 0..12u64 {
        assert!(
            target.fetch(&K { id }.encode()).expect("fetch").is_some(),
            "row {id} is in neither half"
        );
    }
}
