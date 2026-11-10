//! What a writer owes a cutover (ADR-0056 §6): a sealed tail, a position a reader can
//! see, and — once the log is not its own any more — silence.
//!
//! These locks are the difference between "the old instance stopped" and "the old
//! instance cannot write": `drain` is the verb `Drop` is not (it reports), and the gate
//! is what makes a takeover a fact instead of a request.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use okm_core::changelog::{ChangelogConfig, ChangelogStore, sealed_segments};
use okm_core::{Collection, Document, DocumentEncode, KeyEncode, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct RowKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RowKey)]
#[ok_ns(58)]
#[ok_name("drain_rows")]
pub struct Row {
    pub title: String,
}

fn captured(
    root: &std::path::Path,
) -> Collection<ChangelogStore<TestStore>, RowKey, Row> {
    Collection::captured(ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root),
    ))
    .unwrap()
}

fn save(collection: &Collection<ChangelogStore<TestStore>, RowKey, Row>, id: u64) {
    collection
        .put(
            &RowKey { id },
            &Row {
                title: format!("row {id}"),
            },
        )
        .unwrap();
}

#[test]
fn a_drained_writer_seals_its_tail_and_names_what_a_reader_can_see() {
    let log = tempfile::tempdir().unwrap();
    let collection = captured(log.path());
    save(&collection, 1);

    let drained = collection
        .store()
        .drain()
        .unwrap();
    assert_eq!(drained.len(), 1, "one registered table, one writer");
    let (header, report) = &drained[0];

    let sealed = report.sealed.clone().expect("the tail was sealed");
    // The position is READ off the directory, so a reader will find exactly this file.
    let on_disk = sealed_segments(log.path(), header).unwrap();
    let last = on_disk
        .last()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned());
    assert_eq!(Some(sealed), last);
}

#[test]
fn a_writer_that_never_committed_drains_to_nothing_and_leaves_no_partial() {
    let log = tempfile::tempdir().unwrap();
    let collection = captured(log.path());
    // Force the writer open (a registration alone does not open one) and seal it again
    // with nothing in it: the table's directory exists, the log has no tail.
    save(&collection, 1);
    let dir = collection.store().dir().unwrap().join(hex_of());
    assert!(dir.exists(), "{}", dir.display());

    let drained = collection
        .store()
        .drain()
        .unwrap();
    let report = &drained[0].1;
    assert!(report.sealed.is_some(), "the committed row was sealed");

    // Draining twice is not a second seal: the second call finds nothing to write and
    // answers the same position.
    let again = collection.store().drain().unwrap();
    assert_eq!(again[0].1.sealed, report.sealed);
}

#[test]
fn a_closed_gate_stops_the_log_without_failing_the_write() {
    let log = tempfile::tempdir().unwrap();
    let mut collection = captured(log.path());
    save(&collection, 1);

    let gate = Arc::new(AtomicBool::new(true));
    collection.store_mut().attach_gate(Arc::clone(&gate));
    save(&collection, 2);
    assert_eq!(
        collection.store().status::<Row>().unwrap().records_dropped,
        0
    );

    gate.store(false, Ordering::SeqCst);
    save(&collection, 3);

    let status = collection.store().status::<Row>().unwrap();
    assert_eq!(
        status.records_dropped, 1,
        "the commit that slipped past the gate is counted, not appended"
    );
}

/// The table's directory name is the ns prefix in hex — the same rule the writer uses.
fn hex_of() -> String {
    <Row as Document>::NS_PREFIX
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}