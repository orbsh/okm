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

use okm_async::TestStore;
use okm_core::changelog::replay::{reconcile, replay_table};
use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::{Collection, DocumentEncode, KeyEncode, Vocab};
use okm_dynamic::DynamicCollection;
use okm_value::DynamicValue;

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

fn write_log(root: &std::path::Path) -> TestStore {
    let store = TestStore::slatedb_mem();
    let c: Collection<_, K, R> = Collection::captured(ChangelogStore::new(
        store.shared_handle(),
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
    store
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

/// The pass that only the STORE can answer (ADR-0056 §4). Two changes bypass the
/// capture entirely — exactly what a crash between a commit and its append leaves
/// behind — and the reconciliation scan is what repairs them:
///
/// - a delete the log never recorded: the target still holds the row, the store
///   does not, and no segment mentions it at all;
/// - a put the log never recorded: the store has the row, the target does not.
#[test]
fn reconciliation_repairs_what_the_log_never_carried() {
    let log = tempfile::tempdir().expect("log");
    let old = write_log(log.path());

    let dir = std::fs::read_dir(log.path())
        .expect("log root")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one table directory");
    let schema = okm_core::changelog::window::load_sidecar(&dir).expect("sidecar");
    let header = okm_core::changelog::window::header_of(&dir).expect("header");

    let target: DynamicCollection<TestStore> =
        DynamicCollection::new(TestStore::slatedb_mem(), 75, schema.clone(), Vec::new());
    replay_table(&dir, &target).expect("replay");
    assert_eq!(target.scan_document_keys().expect("scan").len(), 2);

    // Changes that never reach a segment: a second collection over the SAME store,
    // with no capturing decorator in the way.
    let plain: Collection<_, K, R> = Collection::new(old.shared_handle());
    assert!(plain.purge(&K { id: 3 }).expect("purge"), "row 3 existed");
    plain
        .put(
            &K { id: 9 },
            &R {
                status: Vocab::of("open"),
                body: "late".to_string(),
            },
        )
        .expect("put the row the log will never mention");

    let report = reconcile(&old, &target, &header, &schema).expect("reconcile");
    assert_eq!(
        report.adopted, 1,
        "row 9: in the store, missing from the log"
    );
    assert_eq!(
        report.removed, 1,
        "row 3: gone from the store, still replayed"
    );
    assert_eq!(report.unchanged, 1, "row 1 needed no write at all");

    let mut bodies: Vec<String> = target
        .scan_document_keys()
        .expect("scan")
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
    assert_eq!(bodies, vec!["a".to_string(), "late".to_string()]);

    // Idempotent: a second pass has nothing to repair.
    let again = reconcile(&old, &target, &header, &schema).expect("reconcile");
    assert_eq!(
        (again.adopted, again.removed),
        (0, 0),
        "a reconciliation that keeps writing is not a reconciliation"
    );
}

/// The dynamic segment is truth (ADR-0045 §4) and is NOT rebuilt by writing the row
/// — a row write only touches declared fields. A replay that skipped those frames
/// would deliver every document without its non-declared fields, silently.
#[test]
fn a_dynamic_field_survives_the_replay() {
    let log = tempfile::tempdir().expect("log");
    let store = TestStore::slatedb_mem();
    let c: Collection<_, K, R> = Collection::captured(ChangelogStore::new(
        store.shared_handle(),
        ChangelogConfig::new(log.path()),
    ))
    .expect("assemble");
    c.put(
        &K { id: 1 },
        &R {
            status: Vocab::of("open"),
            body: "a".to_string(),
        },
    )
    .expect("put");
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("colour".to_string(), DynamicValue::Str("red".to_string()));
    c.put(&K { id: 1 }, &DynamicValue::Obj(extra))
        .expect("a field the declaration does not have");
    // A segment is readable once it is SEALED, and a clean shutdown is what seals it:
    // `Drop for Changelog` flushes the pending frames and renames the tail into a
    // segment. That is the graceful stop a migration relies on — the bulk replay runs
    // while the old instance still serves, and its stop turns the tail into the last
    // window the new instance applies before cutover. A crash leaves a `.partial`,
    // which readers ignore: that lost tail is exactly what reconciliation repairs.
    drop(c);

    let dir = std::fs::read_dir(log.path())
        .expect("log root")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one table directory");
    let schema = okm_core::changelog::window::load_sidecar(&dir).expect("sidecar");
    let target: DynamicCollection<TestStore> =
        DynamicCollection::new(TestStore::slatedb_mem(), 75, schema, Vec::new());

    let report = replay_table(&dir, &target).expect("replay");
    assert_eq!(report.dynamic, 1, "the dynamic segment was carried over");

    let key = target
        .scan_document_keys()
        .expect("scan")
        .into_iter()
        .next()
        .expect("the row");
    let row = target.fetch(&key).expect("fetch").expect("a row");
    assert_eq!(
        row.get("colour"),
        Some(&okm_dynamic::Value::Str("red".to_string())),
        "the segment's field arrived with the row: {row:?}"
    );
    assert_eq!(
        row.get("body"),
        Some(&okm_dynamic::Value::Str("a".to_string()))
    );
}

/// A document can exist with NO primary row: `merge` of nothing but non-declared
/// names leaves a dynamic-only document (ADR-0035). It has to survive both halves of
/// a migration — and, above all, the reconciliation's subtraction must not read it as
/// a lost delete and purge it.
#[test]
fn a_dynamic_only_document_survives_replay_and_reconciliation() {
    let log = tempfile::tempdir().expect("log");
    let store = TestStore::slatedb_mem();
    let c: Collection<_, K, R> = Collection::captured(ChangelogStore::new(
        store.shared_handle(),
        ChangelogConfig::new(log.path()),
    ))
    .expect("assemble");
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("colour".to_string(), DynamicValue::Str("blue".to_string()));
    c.put(&K { id: 7 }, &DynamicValue::Obj(extra))
        .expect("a segment with no row");
    drop(c);

    let dir = std::fs::read_dir(log.path())
        .expect("log root")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one table directory");
    let schema = okm_core::changelog::window::load_sidecar(&dir).expect("sidecar");
    let header = okm_core::changelog::window::header_of(&dir).expect("header");
    let target: DynamicCollection<TestStore> =
        DynamicCollection::new(TestStore::slatedb_mem(), 75, schema.clone(), Vec::new());

    let report = replay_table(&dir, &target).expect("replay");
    assert_eq!(report.dynamic, 1, "the segment is the whole document here");

    let pkey = K { id: 7 }.encode();
    let row = target.fetch(&pkey).expect("fetch").expect("the document");
    assert_eq!(
        row.get("colour"),
        Some(&okm_dynamic::Value::Str("blue".to_string()))
    );

    // The subtraction half must count the dynamic face as presence, or this
    // document is "gone from the store" and gets purged on every migration.
    let rec = reconcile(&store, &target, &header, &schema).expect("reconcile");
    assert_eq!((rec.adopted, rec.removed), (0, 0), "{rec:?}");
    let row = target
        .fetch(&pkey)
        .expect("fetch")
        .expect("still there after reconciliation");
    assert_eq!(
        row.get("colour"),
        Some(&okm_dynamic::Value::Str("blue".to_string()))
    );
}
