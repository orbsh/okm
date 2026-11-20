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
use okm_core::changelog::{last_sealed, table_dir, ChangelogConfig, ChangelogStore, LakeBaseline};
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};
use okm_lake::baseline::verify_at;
use okm_lake::{drain, DrainConfig};

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
    Collection::captured(ChangelogStore::new(
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
    assert_eq!(still.files, 1, "version 0 held one window, whatever came after");
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
