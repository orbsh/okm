//! Replay (ADR-0056 §5–§6): a log directory in, a store built by THIS build out.
//!
//! The claim under test is not “the rows arrive” — it is that they arrive through
//! the write path: the target allocates its own vocabulary ids, rebuilds its own
//! faces, and a removal stays removed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::{Collection, DocumentEncode, KeyEncode, TestStore, Vocab};
use okm_dynamic::DynamicCollection;
use okm_parquet::replay::replay_table;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

/// A vocabulary field: the payload holds an id, the log carries the label face,
/// so replay has to go `id → label → the target's own id`.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(75)]
#[ok_name("replayed_rows")]
pub struct R {
    pub status: Vocab<u8>,
    pub body: String,
}

fn write_log(root: &std::path::Path) {
    let c: Collection<_, K, R> = Collection::captured(ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root),
    ))
    .expect("assemble");

    for (id, status, body) in [(1u64, "open", "a"), (2, "closed", "b"), (3, "open", "c")] {
        c.put(
            &K { id },
            &R {
                // Built from the LABEL: the id is the write path's to allocate, and
                // replay has to respect the same rule on the other side.
                status: Vocab::of(status),
                body: body.to_string(),
            },
        )
        .expect("put");
    }
    c.purge(&K { id: 2 }).expect("purge");
}

#[test]
fn replay_rewrites_the_rows_through_the_targets_write_path() {
    let log = tempfile::tempdir().expect("log");
    write_log(log.path());

    let dir = std::fs::read_dir(log.path())
        .expect("log root")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one table directory");

    // The target: a fresh store, declared from the sidecar the log carries, so this
    // is the standalone shape — no application types reach the replayer.
    let schema = okm_core::changelog::window::load_sidecar(&dir).expect("sidecar");
    let target: DynamicCollection<TestStore> =
        DynamicCollection::new(TestStore::slatedb_mem(), 75, schema, Vec::new());

    let report = replay_table(&dir, &target).expect("replay");
    assert_eq!(report.puts, 3, "every captured put was applied");
    assert_eq!(report.deletes, 1, "the purge is a removal, not a rewrite");

    // Read back through the target's own decode path: the two surviving rows, with
    // the label the log carried (the id is the target's).
    let keys = target.scan_document_keys().expect("scan");
    assert_eq!(keys.len(), 2, "the purged row is gone: {keys:?}");
    let mut bodies: Vec<String> = keys
        .iter()
        .map(|k| {
            let row = target.fetch(k).expect("fetch").expect("a row");
            match row.get("body") {
                Some(okm_dynamic::Value::Str(s)) => s.clone(),
                other => panic!("body: {other:?}"),
            }
        })
        .collect();
    bodies.sort();
    assert_eq!(bodies, vec!["a".to_string(), "c".to_string()]);
}