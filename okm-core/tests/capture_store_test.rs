//! Capture at the store boundary (ADR-0056 §2): one switch for the whole store.
//!
//! Three claims, and each one is a hole the collection-level hook left open:
//! the log fills without `with_changelog`, a caller-owned batch committed straight
//! to the engine is still captured, and a table the operator excluded stays out.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::{ChangelogConfig, ChangelogStore, read_sealed, sealed_segments};
use okm_core::{Batch, Collection, Document, DocumentEncode, KeyEncode, TestStore, VirtualStorage};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(73)]
#[ok_name("captured_rows")]
pub struct Row {
    pub level: u32,
}

/// The table the operator takes OUT of the switch.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(74)]
#[ok_name("excluded_rows")]
pub struct Quiet {
    pub level: u32,
}

const NS: &[u8] = <Row as Document>::NS_PREFIX;
const QUIET_NS: &[u8] = <Quiet as Document>::NS_PREFIX;

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(root))
}

/// The declaration is registered at assembly, so the table directory carries its
/// sidecar and the segments carry its name — before the first write.
#[test]
fn the_store_captures_without_the_collection_asking() {
    let root = tempfile::tempdir().expect("root");
    {
        // No `with_changelog`: the store has the switch, the collection does not.
        let c: Collection<_, K, Row> =
            Collection::captured(store(root.path())).expect("assemble");
        c.put(&K { id: 1 }, &Row { level: 3 }).expect("put");
        c.put(&K { id: 2 }, &Row { level: 4 }).expect("put");
    }

    let dir = root
        .path()
        .join(NS.iter().fold(String::new(), |mut s, b| {
            s.push_str(&format!("{b:02x}"));
            s
        }));
    let sidecar = dir.join(format!(
        "schema-v{}.json",
        <Row as Document>::LAYOUT_VERSION
    ));
    assert!(sidecar.exists(), "registration writes the sidecar");

    let segments = sealed_segments(root.path(), NS).expect("list segments");
    assert_eq!(segments.len(), 1, "a clean shutdown seals the segment");
    let frames = read_sealed(&segments[0]).expect("sealed segment reads");
    assert_eq!(frames.len(), 2, "one frame per commit, with no hook on the collection");

    let info = okm_core::changelog::segment_info(&segments[0]).expect("info");
    assert_eq!(
        info.meta.expect("stamped").name,
        "captured_rows",
        "the registered name reached the segment"
    );
}

/// The hole this change closes: `save_with_prev_into` hands the caller a batch and
/// the caller commits it to the engine itself, which no collection-level hook can
/// see. At the boundary it is an ordinary commit.
#[test]
fn a_caller_owned_batch_is_captured_at_the_store() {
    let root = tempfile::tempdir().expect("root");
    {
        let c: Collection<_, K, Row> =
            Collection::captured(store(root.path())).expect("assemble");
        let mut batch = Batch::default();
        c.save_with_prev_into(&mut batch, &K { id: 9 }, &Row { level: 7 }, None)
            .expect("into the caller's batch");
        // Straight to the engine: the collection never sees a commit.
        c.store().commit_batch(batch).expect("commit");
    }

    let segments = sealed_segments(root.path(), NS).expect("list segments");
    assert_eq!(segments.len(), 1, "the shutdown sealed it");
    let frames = read_sealed(&segments[0]).expect("reads");
    assert_eq!(frames.len(), 1, "the store saw the write the collection did not");
}

/// An excluded ns is the operator's exception: it registers no declaration and
/// writes no segment, so a chatty table nobody will ever replay costs nothing.
#[test]
fn a_table_outside_the_switch_writes_no_log() {
    let root = tempfile::tempdir().expect("root");
    {
        let s = store(root.path()).exclude(74);
        let q: Collection<_, K, Quiet> = Collection::captured(s).expect("assemble");
        q.put(&K { id: 1 }, &Quiet { level: 1 }).expect("put");
    }

    assert!(
        sealed_segments(root.path(), QUIET_NS)
            .expect("list segments")
            .is_empty(),
        "the excluded table captured nothing"
    );
}

/// A baseline is what makes a log enabled mid-life complete: the rows that exist
/// before it are in the snapshot, and every change after it is in a segment
/// numbered ABOVE the snapshot (ADR-0056 §3). Ordering is the point — a delta
/// numbered below a baseline is covered by the snapshot and a replay skips it.
#[test]
fn a_baseline_makes_a_mid_life_log_complete() {
    let root = tempfile::tempdir().expect("root");
    {
        let c: Collection<_, K, Row> = Collection::captured(store(root.path())).expect("assemble");
        c.put(&K { id: 1 }, &Row { level: 3 }).expect("put");

        assert!(
            c.store().ensure_baseline::<K, Row>().expect("baseline"),
            "the table had none"
        );
        assert!(
            !c.store().ensure_baseline::<K, Row>().expect("again"),
            "a table is never given a second snapshot"
        );

        // A write AFTER the snapshot: it must land in a segment numbered above it.
        c.put(&K { id: 2 }, &Row { level: 4 }).expect("put");
    }

    let baselines = okm_core::changelog::baseline_segments(root.path(), NS).expect("baselines");
    assert_eq!(baselines.len(), 1, "exactly one snapshot");
    let info = okm_core::changelog::segment_info(&baselines[0]).expect("info");
    assert_eq!(
        info.meta.expect("stamped").kind,
        okm_core::changelog::KIND_BASELINE,
        "the header says what the name says"
    );
    assert_eq!(info.frames, 1, "one frame per 1024 rows, so one here");
    assert_eq!(info.ordinal, 1, "written after the delta that came before it");

    let frames = read_sealed(&baselines[0]).expect("the snapshot reads");
    let ops: usize = frames.iter().map(|f| f.0.len()).sum();
    assert_eq!(ops, 1, "the row that existed when the snapshot was taken");

    let deltas = sealed_segments(root.path(), NS).expect("deltas");
    assert_eq!(deltas.len(), 2, "one delta before the snapshot, one after");
    let names: Vec<String> = deltas
        .iter()
        .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
        .collect();
    assert!(names[0].starts_with("00000000"), "the pre-snapshot delta: {names:?}");
    assert!(
        names[1].starts_with("00000002"),
        "the write after the snapshot is numbered above it: {names:?}"
    );
}