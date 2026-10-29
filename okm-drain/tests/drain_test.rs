//! The drain end to end: a collection writes with the change log on, the segments
//! seal, and the drainer turns them into one Parquet window that a plain Parquet
//! reader can open — then refuses to drain the same window twice.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::ChangelogConfig;
use okm_core::{Collection, DocumentEncode, KeyEncode, TestStore};
use okm_drain::{DrainConfig, drain};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub org: u32,
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(71)]
#[ok_name("drained_rows")]
pub struct R {
    pub level: u32,
    pub name: String,
}

fn collection(root: &std::path::Path) -> Collection<TestStore, K, R> {
    Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(root))
        .expect("the log opens")
}

const K1: K = K { org: 1, id: 10 };
const K2: K = K { org: 1, id: 11 };

#[test]
fn one_window_becomes_one_parquet_file_and_the_watermark_moves() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("out");
    {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 3,
                name: "alpha".into(),
            },
        )
        .expect("put");
        c.put(
            &K2,
            &R {
                level: 4,
                name: "beta".into(),
            },
        )
        .expect("put");
    } // Drop seals the open segment: a clean exit means the window is drainable.

    let cfg = DrainConfig::new(out.path(), "test");
    let report = drain(root.path(), &cfg).expect("drain");
    assert_eq!(report.tables, 1);
    assert_eq!(report.segments, 1, "the sealed segment is the window");
    assert_eq!(report.rows, 2, "one row per captured put");
    assert_eq!(report.files.len(), 1);

    // A plain Parquet reader, no okm in the path: the columns and the values are
    // what the declaration said they are.
    let file = std::fs::File::open(&report.files[0]).expect("open");
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("reader")
        .build()
        .expect("build");
    let batch = reader.next().expect("one batch").expect("batch");
    assert_eq!(batch.num_rows(), 2);
    let names: Vec<String> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    assert_eq!(
        names,
        ["org", "id", "level", "name"],
        "keys first, then the row"
    );

    // Second run: the watermark holds, so the same window is not drained twice.
    let again = drain(root.path(), &cfg).expect("second drain");
    assert_eq!(again.rows, 0);
    assert_eq!(again.segments, 0);
    assert!(again.files.is_empty());
}

#[test]
fn a_window_with_a_delete_is_refused_rather_than_misrepresented() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("out");
    // One window holding both ops: the put and the purge that removes it. A
    // window is history, and the declaration has no column for a removal — the
    // right answer is to stop, not to hand a reader a row that no longer exists.
    {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 3,
                name: "alpha".into(),
            },
        )
        .expect("put");
        assert!(c.purge(&K1).expect("purge"), "the row existed");
    }
    let cfg = DrainConfig::new(out.path(), "test");
    let err = drain(root.path(), &cfg).expect_err("a delete cannot become a row");
    // A refused window leaves nothing behind: no Parquet file, and the watermark
    // still points where it did, so the next run sees the same window.
    let artefacts: Vec<_> = std::fs::read_dir(out.path())
        .expect("out dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    assert!(
        artefacts.is_empty(),
        "nothing half-committed: {artefacts:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("delete") && text.contains("compact"),
        "the refusal must name the op and the way out, got: {text}"
    );
}

#[test]
fn a_table_without_a_sidecar_stops_the_drain() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("out");
    {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 3,
                name: "alpha".into(),
            },
        )
        .expect("put");
    }
    // The bytes without the declaration: readable, not columns.
    for dir in std::fs::read_dir(root.path()).expect("root dirs") {
        let dir = dir.expect("entry").path();
        if dir.is_dir() {
            for f in std::fs::read_dir(&dir).expect("table dir") {
                let p = f.expect("entry").path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("schema-v"))
                {
                    std::fs::remove_file(p).expect("remove sidecar");
                }
            }
        }
    }
    let err = drain(root.path(), &DrainConfig::new(out.path(), "test"))
        .expect_err("no sidecar means no columns");
    assert!(err.to_string().contains("sidecar"), "got: {err}");
}
