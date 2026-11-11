//! The log's maintenance verbs, driven as a real process (ADR-0056 §6).
//!
//! `status` and `compact` are the operator's side of the log, and they must work on a
//! directory with no application and no Rust types — which is what a process boundary
//! proves better than a function call does.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::process::Command;

use okm_core::changelog::{ChangelogConfig, ChangelogStore, apply_log, table_dir};
use okm_core::{Collection, Document, DocumentEncode, FjallStore, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct RowKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RowKey)]
#[ok_ns(63)]
#[ok_name("cli_rows")]
pub struct Row {
    pub title: String,
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_okm-log"))
}

#[test]
fn status_reports_the_log_and_compact_checkpoints_it() {
    let log = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let header = <Row as Document>::NS_PREFIX.to_vec();

    // A capture that writes one segment per commit, so the log has something to report.
    {
        let store = FjallStore::open(data.path(), "okm").unwrap();
        let cfg = ChangelogConfig {
            segment_bytes: 1,
            compact: None,
            ..ChangelogConfig::new(log.path())
        };
        let app = Collection::<_, RowKey, Row>::captured(ChangelogStore::new(store, cfg)).unwrap();
        for id in 0..6 {
            app.put(
                &RowKey { id },
                &Row {
                    title: format!("row {id}"),
                },
            )
            .unwrap();
        }
    }

    let out = bin()
        .args(["status", "--root"])
        .arg(log.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("segments 6"), "{text}");
    assert!(text.contains("baseline none"), "{text}");
    assert!(text.contains("claim: none"), "{text}");
    assert!(text.contains("policy"), "{text}");

    let out = bin()
        .args(["compact", "--root"])
        .arg(log.path())
        .args(["--store"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("baseline"), "{text}");
    assert!(text.contains("removed"), "{text}");

    // The log is now one baseline and nothing else…
    let out = bin()
        .args(["status", "--root"])
        .arg(log.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains(".baseline."), "{text}");
    assert!(text.contains("segments 1"), "{text}");

    // …and a replay off it still holds every row.
    let restored_dir = tempfile::tempdir().unwrap();
    let target = FjallStore::open(restored_dir.path(), "okm").unwrap();
    apply_log(log.path(), &header, &target, None).unwrap();
    let restored = Collection::<_, RowKey, Row>::new(target);
    for id in 0..6 {
        assert_eq!(
            restored.get(&RowKey { id }).unwrap().unwrap().title,
            format!("row {id}")
        );
    }
    assert!(
        std::fs::read_dir(table_dir(log.path(), &header))
            .unwrap()
            .count()
            > 0
    );
}

#[test]
fn a_live_claim_is_refused_without_force() {
    let log = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let header = <Row as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(log.path(), &header);

    // A claim that looks live: `writing`, and the file was just written.
    std::fs::create_dir_all(&dir).unwrap();
    okm_core::changelog::owner::write(
        &dir,
        &okm_core::changelog::owner::Owner {
            state: okm_core::changelog::owner::State::Writing,
            epoch: 1,
            holder: "a-holder".to_string(),
            data: "/data/x".to_string(),
            position: None,
        },
    )
    .unwrap();

    let out = bin()
        .args(["compact", "--root"])
        .arg(log.path())
        .args(["--store"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "a live claim must be refused");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is writing this log"), "{err}");

    // Nothing was touched.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}
#[test]
fn the_log_and_the_store_cannot_be_the_same_directory() {
    let dir = tempfile::tempdir().unwrap();
    // The store path does not exist yet: the refusal must arrive before anything is
    // created, so a mistaken invocation leaves no trace.
    let nested = dir.path().join("data");

    for store in [dir.path().to_path_buf(), nested.clone()] {
        let out = bin()
            .args(["compact", "--root"])
            .arg(dir.path())
            .args(["--store"])
            .arg(&store)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{} must be refused", store.display());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("must be separate directories"), "{err}");
    }
    assert!(!nested.exists(), "the refusal created nothing");
}
