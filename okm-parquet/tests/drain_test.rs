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

use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::{Collection, DocumentEncode, KeyEncode, TestStore};
use okm_parquet::{DrainConfig, drain};
use arrow::array::Array;
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

fn collection(root: &std::path::Path) -> Collection<ChangelogStore<TestStore>, K, R> {
    Collection::captured(ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root),
    ))
    .expect("the log opens")
}

const K1: K = K { org: 1, id: 10 };
const K2: K = K { org: 1, id: 11 };

/// The second table of the two-depth test: same key, DIFFERENT ns, so it has its
/// own directory and its own segment numbering — which is what the watermark
/// must not confuse with the first table's.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(72)]
#[ok_name("lagging_rows")]
pub struct LaggingRow {
    pub level: u32,
}

fn lagging_collection(root: &std::path::Path) -> Collection<ChangelogStore<TestStore>, K, LaggingRow> {
    Collection::captured(ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root),
    ))
    .expect("the log opens")
}

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

    // The manifest beside it says where the file came from — the answer a sink's
    // provenance entry and an operator's "did window N land" both need.
    let manifest_path = report.files[0].with_extension("manifest.json");
    let manifest: okm_parquet::WindowManifest =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("manifest"))
            .expect("manifest parses");
    assert_eq!(manifest.rows, 2);
    assert_eq!(
        manifest.layout_version,
        <R as okm_core::Document>::LAYOUT_VERSION
    );
    assert_eq!(manifest.segments.len(), 1);
    assert_eq!(
        manifest.columns.len(),
        4,
        "keys first, then the row, as the file has them"
    );
    assert_eq!(
        manifest.columns[0],
        ("org".to_string(), "UInt32".to_string(), false),
        "a key column: typed, not nullable"
    );
    assert!(manifest.columns[3].2, "a payload column is nullable");

    // The exported table is a TABLE, not a pile of files: Iceberg metadata over the
    // data directory, with the column ids stamped in the Parquet and the file listed
    // in a snapshot, so a catalog (or pyiceberg) reads it with no name mapping
    // (ADR-0056 §8).
    assert_eq!(report.metadata, 1, "one table, one metadata file");
    let table_dir = report.files[0].parent().unwrap().parent().unwrap().to_path_buf();
    let metadata = table_dir.join("metadata").join("v1.metadata.json");
    let text = std::fs::read_to_string(&metadata).expect("iceberg metadata");
    assert!(
        text.contains("\"format-version\": 2"),
        "an Iceberg v2 table: {text}"
    );
    assert!(
        text.contains("manifest-list"),
        "the snapshot points at a manifest list (Iceberg v2 keeps file names in the \
         manifest, not in metadata.json): {text}"
    );
    // The data file is named in the MANIFEST — the avro the snapshot points at. An
    // exported table that lists no file is an empty table with extra steps.
    let file_name = report.files[0].file_name().unwrap().to_string_lossy().to_string();
    let manifest_bytes: Vec<u8> = std::fs::read_dir(metadata.parent().unwrap())
        .expect("metadata dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "avro"))
        .filter_map(|p| std::fs::read(p).ok())
        .flatten()
        .collect();
    assert!(
        manifest_bytes
            .windows(file_name.len())
            .any(|w| w == file_name.as_bytes()),
        "the manifest lists the file that was just written ({file_name})"
    );
    assert!(
        text.contains("\"name\":\"level\""),
        "the columns are read from the Parquet, not restated by the caller: {text}"
    );
    // The table's NAME is the catalog's business — Iceberg metadata carries none —
    // so the directory is what names it, and that is what a reader is pointed at.
    assert_eq!(
        std::fs::read_to_string(table_dir.join("metadata").join("version-hint.text"))
            .expect("version hint"),
        "1",
        "a directory reader (DuckDB) finds the current metadata through it"
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

/// The reader reports what the log says; only the sink refuses. A removal is a
/// fact — a replay applies it in order — while a column file is what cannot hold
/// it, so the refusal belongs to the sink and not to the window (ADR-0056 §5).
#[test]
fn a_removal_is_read_and_only_the_parquet_sink_refuses_it() {
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
        assert!(c.purge(&K1).expect("purge"), "the row existed");
    }
    let dir = std::fs::read_dir(root.path())
        .expect("root dirs")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one table directory");

    // The reading half is core's now: the window join lives beside the segments it
    // reads (ADR-0056 §8), so this test calls it where it lives.
    let window_cfg = okm_core::changelog::window::WindowConfig::default();
    let window = okm_core::changelog::window::read_window(&dir, &window_cfg)
        .expect("read")
        .expect("a window");
    assert_eq!(window.rows.len(), 1, "the put is still a row in history");
    assert!(
        window
            .deletes
            .iter()
            .any(|(slot, key)| *slot == 0 && key.len() == window.schema.key_len),
        "the removal is on the primary face: {:?}",
        window.deletes
    );

    let target = out.path().join("x.parquet");
    let err = okm_parquet::write_parquet(&window, &target).expect_err("a column cannot hold it");
    assert!(err.to_string().contains("delete"), "got: {err}");
    assert!(!target.exists(), "nothing half-written");
}

/// Two tables at different depths: the one that is further ahead must not move
/// the lagging table's position. Segment numbers restart per table, so a
/// consumer-named watermark file would skip the lagging table silently and for
/// good — the reason the position is per `(consumer, table)` (ADR-0056 §8).
#[test]
fn a_lagging_table_is_not_skipped_by_the_one_ahead() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("out");

    // Table 0x0047 (ns 71) gets TWO sealed segments — one per clean drop.
    for level in [1u32, 2] {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level,
                name: "alpha".into(),
            },
        )
        .expect("put");
    }

    // Table 0x0048 (ns 72) gets one, so it sits at ordinal 1 while the first
    // table has reached ordinal 2.
    {
        let c = lagging_collection(root.path());
        c.put(&K1, &LaggingRow { level: 9 }).expect("put");
    }

    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    assert_eq!(report.tables, 2);
    assert_eq!(
        report.segments, 3,
        "two segments for the table ahead, one for the lagging one"
    );
    let files: Vec<String> = report
        .files
        .iter()
        .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 2, "one window file per table: {files:?}");
    assert!(
        files.iter().any(|f| f.starts_with("0048-")),
        "the lagging table still drains: {files:?}"
    );

    // And both positions are their own file, so a second run is a no-op for both.
    let again = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("second drain");
    assert_eq!(again.segments, 0, "nothing left for either table");
    assert!(again.files.is_empty());
}

/// The open half of a document — the names the declaration does not have — has no
/// column, and a window that wrote only declared fields would hand a reader every row
/// minus part of itself (ADR-0045 §4). It travels as ONE `variant` column beside the
/// rows, null where a row has no segment.
#[test]
fn a_documents_open_fields_travel_as_a_variant_column() {
    let root = tempfile::tempdir().expect("log");
    let out = tempfile::tempdir().expect("lake");
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
        let mut open = std::collections::BTreeMap::new();
        open.insert(
            "note".to_string(),
            okm_core::model::obj_dynamic::DynamicValue::Str("first".to_string()),
        );
        c.merge(&K1, &open).expect("a field the declaration does not have");
    }

    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    assert_eq!(report.open_fields, 1, "one segment carried, not counted and dropped");

    let file = std::fs::File::open(&report.files[0]).expect("open");
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("reader")
        .build()
        .expect("build");
    let batch = reader.next().expect("one batch").expect("batch");
    let schema = batch.schema();
    let last = schema.field(schema.fields().len() - 1);
    assert_eq!(last.name(), "open", "the variant column comes after the fields");
    assert_eq!(
        last.metadata().get("ARROW:extension:name").map(String::as_str),
        Some("parquet.variant"),
        "a reader that knows variants is told what this column is"
    );
    assert!(last.is_nullable(), "a row with no segment has nothing here");

    let column = batch
        .column(schema.fields().len() - 1)
        .as_any()
        .downcast_ref::<arrow::array::BinaryArray>()
        .expect("binary");
    assert_eq!(column.len(), 2);
    assert_eq!(column.null_count(), 1, "one row has a segment, one does not");
}
