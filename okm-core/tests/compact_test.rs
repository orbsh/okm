//! Compaction: a new baseline covers the segments below it, and a replay after a
//! compaction still rebuilds the table (ADR-0056 §6).
//!
//! The whole point is that pruning is not a loss: the log's job is to be applied, and a
//! baseline is the state at a segment number, so everything numbered below it is covered.
//! What the test has to prove is the thing that could break — replay across the seam.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::{ChangelogConfig, ChangelogStore, apply_log, table_dir};
use okm_core::{Collection, Document, DocumentEncode, KeyEncode, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct RowKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RowKey)]
#[ok_ns(61)]
#[ok_name("compact_rows")]
pub struct Row {
    pub title: String,
}

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig {
            // One segment per commit, so the test can see the cleanup.
            segment_bytes: 1,
            ..ChangelogConfig::new(root)
        },
    )
}

fn segments(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".segment")
        })
        .count()
}

#[test]
fn a_compaction_prunes_what_the_baseline_covers_and_replay_still_works() {
    let log = tempfile::tempdir().unwrap();
    let header = <Row as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(log.path(), &header);

    let writer = store(log.path());
    writer.register::<RowKey, Row>().unwrap();
    let app = Collection::<_, RowKey, Row>::captured(writer).unwrap();
    for id in 0..12 {
        app.put(
            &RowKey { id },
            &Row {
                title: format!("row {id}"),
            },
        )
        .unwrap();
    }
    let before = segments(&dir);
    assert!(before >= 12, "one segment per commit, saw {before}");

    let compacted = app.store().compact(0).unwrap();
    assert_eq!(compacted.len(), 1);
    let report = &compacted[0].1;
    assert!(report.baseline.is_some(), "a baseline was written");
    // Closing the writer can seal a footer-only delta, so the count is at least `before`;
    // what the assertion pins is that nothing below the baseline survives.
    assert!(
        report.removed >= before,
        "everything below it is covered: {} of {before}",
        report.removed
    );
    assert!(report.freed > 0, "the test should be able to see the size");
    let left = segments(&dir);
    assert!(left >= 1, "the baseline is there");
    assert!(
        std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                let name = e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .into_owned();
                name.ends_with(".segment") && !name.contains(".baseline.")
            })
            .count()
            == 0,
        "no delta survives below the baseline"
    );

    // The log is still a log: what is written above the baseline is a delta.
    for id in 100..104 {
        app.put(
            &RowKey { id },
            &Row {
                title: format!("after {id}"),
            },
        )
        .unwrap();
    }
    assert!(
        segments(&dir) > 1,
        "writing after a compaction adds segments"
    );

    // And a replay into an empty store rebuilds every row, from both sides of the seam.
    let target = TestStore::slatedb_mem();
    let applied = apply_log(log.path(), &header, &target, None).unwrap();
    assert!(
        applied.ops >= 16,
        "12 covered + 4 above, saw {}",
        applied.ops
    );
    let restored = Collection::<_, RowKey, Row>::new(target);
    for id in 0..12 {
        assert_eq!(
            restored.get(&RowKey { id }).unwrap().unwrap().title,
            format!("row {id}")
        );
    }
    for id in 100..104 {
        assert_eq!(
            restored.get(&RowKey { id }).unwrap().unwrap().title,
            format!("after {id}")
        );
    }
}

#[test]
fn keep_leaves_a_window_of_history_under_the_baseline() {
    let log = tempfile::tempdir().unwrap();
    let header = <Row as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(log.path(), &header);

    let writer = store(log.path());
    writer.register::<RowKey, Row>().unwrap();
    let app = Collection::<_, RowKey, Row>::captured(writer).unwrap();
    for id in 0..10 {
        app.put(&RowKey { id }, &Row { title: "x".into() }).unwrap();
    }
    let before = segments(&dir);

    let compacted = app.store().compact(3).unwrap();
    assert!(
        compacted[0].1.removed + 3 >= before,
        "all but three removed: {} of {before}",
        compacted[0].1.removed
    );
    let deltas = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            let name = e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .into_owned();
            name.ends_with(".segment") && !name.contains(".baseline.")
        })
        .count();
    assert_eq!(deltas, 3, "three kept below the baseline");
}

#[test]
fn capture_off_has_nothing_to_compact() {
    let log = tempfile::tempdir().unwrap();
    let off = ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig {
            location: okm_core::changelog::Location::Off,
            ..ChangelogConfig::new(log.path())
        },
    );
    off.register::<RowKey, Row>().unwrap();
    assert!(off.compact(0).unwrap().is_empty());
}
