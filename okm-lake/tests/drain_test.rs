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

use arrow::array::Array;
use okm_async::TestStore;
use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::{Collection, DocumentEncode, KeyEncode};
use okm_lake::{DrainConfig, drain};
use okm_value::{Dict, Variant};
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

/// The table directory a written file belongs to: walk up until the Delta log is there, so a
/// partitioned path (`data/org=1/…`) and a plain one are answered the same way.
fn table_of(file: &std::path::Path) -> std::path::PathBuf {
    let mut dir = file.parent().expect("a file has a parent").to_path_buf();
    while !dir.join("_delta_log").is_dir() {
        dir = dir
            .parent()
            .expect("a table directory above the file")
            .to_path_buf();
    }
    dir
}

fn collection(root: &std::path::Path) -> Collection<ChangelogStore<TestStore>, K, R> {
    Collection::captured(ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root),
    ))
    .expect("the log opens")
}

const K1: K = K { org: 1, id: 10 };
/// A key in ANOTHER partition: `org` is the column the partition tests split on.
const K3: K = K { org: 2, id: 20 };
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

fn lagging_collection(
    root: &std::path::Path,
) -> Collection<ChangelogStore<TestStore>, K, LaggingRow> {
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
        [
            "org", "id", "level", "name", "okm_op", "okm_slot", "okm_seq"
        ],
        "keys first, then the row, then what the log said about it"
    );

    // The manifest beside it says where the file came from — the answer a sink's
    // provenance entry and an operator's "did window N land" both need.
    let manifest_path = report.files[0].with_extension("manifest.json");
    let manifest: okm_lake::WindowManifest =
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
        7,
        "keys first, then the row, then the three change columns"
    );
    assert_eq!(
        manifest.columns[4],
        ("okm_op".to_string(), "Utf8".to_string(), false),
        "a change column rides beside the declaration, not inside it"
    );
    assert_eq!(
        manifest.columns[0],
        ("org".to_string(), "UInt32".to_string(), false),
        "a key column: typed, not nullable"
    );
    assert!(manifest.columns[3].2, "a payload column is nullable");

    // The exported table is a TABLE, not a pile of files: a Delta log over the data
    // directory, so a reader pointed at the DIRECTORY finds the schema and the files
    // with nothing registered anywhere (ADR-0056 §8).
    assert_eq!(report.metadata, 1, "one table, one commit");
    let table_dir = report.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let log = table_dir.join("_delta_log");
    let version = log.join(format!("{:020}.json", 0));
    let text = std::fs::read_to_string(&version).expect("the first Delta commit");
    let actions: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("an action is JSON"))
        .collect();
    assert_eq!(
        actions[0]["protocol"]["minReaderVersion"], 1,
        "plain parquet with no feature a reader has to know: {text}"
    );
    // The schema is read from the Parquet footer, not restated by the caller, and it
    // is by NAME: Delta has no field ids, so a column's identity is its name.
    let schema: serde_json::Value = serde_json::from_str(
        actions[1]["metaData"]["schemaString"]
            .as_str()
            .expect("schema"),
    )
    .expect("the schema string is JSON");
    let names: Vec<&str> = schema["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(
        names,
        [
            "org", "id", "level", "name", "okm_op", "okm_slot", "okm_seq"
        ],
        "columns from the file"
    );
    let file_name = report.files[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        actions
            .iter()
            .any(|a| a["add"]["path"].as_str() == Some(&format!("data/{file_name}"))),
        "the commit adds the file that was just written, by a path RELATIVE to the \
         table: {text}"
    );
    assert!(
        !text.contains(&table_dir.to_string_lossy().to_string()),
        "no absolute path anywhere in the log, so the table can be moved: {text}"
    );

    // Second run: the watermark holds, so the same window is not drained twice.
    let again = drain(root.path(), &cfg).expect("second drain");
    assert_eq!(again.rows, 0);
    assert_eq!(again.segments, 0);
    assert!(again.files.is_empty());
}

#[test]
fn a_window_with_a_delete_writes_a_change_row() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("out");
    // One window holding both ops: the put and the purge that removes it. A window is
    // history, and the file says so — two change rows in the order the log carried them,
    // the removal holding the key with the declared columns NULL (ADR-0058).
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
    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    assert_eq!(report.rows, 1, "the put is still a row in history");
    assert_eq!(
        report.deletes, 1,
        "the removal is a change row, not a refusal"
    );
    assert_eq!(report.files.len(), 1);

    let file = std::fs::File::open(&report.files[0]).expect("open");
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("reader")
        .build()
        .expect("build");
    let batch = reader.next().expect("one batch").expect("batch");
    assert_eq!(batch.num_rows(), 2, "put then delete: two changes");

    let array = |name: &str| batch.column_by_name(name).expect(name);
    let ops = array("okm_op")
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .expect("okm_op is text");
    assert_eq!((ops.value(0), ops.value(1)), ("put", "del"));
    let seqs = array("okm_seq")
        .as_any()
        .downcast_ref::<arrow::array::UInt32Array>()
        .expect("okm_seq is a u32");
    assert_eq!(
        (seqs.value(0), seqs.value(1)),
        (0, 1),
        "the order the log carried, not a rule invented by the sink"
    );
    let slots = array("okm_slot")
        .as_any()
        .downcast_ref::<arrow::array::UInt16Array>()
        .expect("okm_slot is a u16");
    assert!(slots.is_null(0), "a put touches no face");
    assert_eq!(slots.value(1), 0, "slot 0: the whole row is gone");
    let level = array("level");
    assert!(
        !level.is_null(0) && level.is_null(1),
        "the removal has no declared value to state, so it states none"
    );

    // The Delta log covers the file that now holds a removal: one version, one `add`,
    // no protocol feature for a reader to learn.
    let table_dir = report.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let version = table_dir.join("_delta_log").join(format!("{:020}.json", 0));
    let text = std::fs::read_to_string(&version).expect("the first Delta commit");
    assert!(text.contains("\"add\""), "the change file is in the log");
    assert!(
        !text.contains("deletionVector") && !text.contains("remove"),
        "a change row needs no protocol feature: {text}"
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

/// The reader reports what the log says and the sink writes it: a removal is a fact —
/// a replay applies it in order, a change table states it as a row (ADR-0058).
#[test]
fn a_removal_is_read_and_written_as_a_change_row() {
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

    // The reading half is core's: the window join lives beside the segments it reads
    // (ADR-0056 §8), so this test calls it where it lives.
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
    assert_eq!(
        window.ops.len(),
        2,
        "both ops, in the order the log carried them: {:?}",
        window.ops
    );

    let (batch, projection) = okm_lake::project(&window).expect("a change row states it");
    assert_eq!(projection.rows, 2);
    assert_eq!(projection.deletes, 1);
    assert_eq!(projection.open_only, 0);
    // The batch is the file's content; writing it is the drain's business.
    assert_eq!(batch.num_rows(), 2);
    let target = out.path().join("x.parquet");
    okm_lake::snapshot::parquet_io::write_batch(&batch, &target).expect("write");
    assert!(target.exists(), "the file a change table needs");
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
        .map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
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
        let mut open = Dict::new();
        open.insert(
            "note".to_string(),
            okm_value::Variant::Str("first".to_string()),
        );
        c.put(&K1, &Variant::Map(open))
            .expect("a field the declaration does not have");
    }

    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    assert_eq!(
        report.open_fields, 1,
        "one segment carried, not counted and dropped"
    );

    let file = std::fs::File::open(&report.files[0]).expect("open");
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("reader")
        .build()
        .expect("build");
    let batch = reader.next().expect("one batch").expect("batch");
    let schema = batch.schema();
    // Three change columns follow the variant, so it is no longer the last field.
    let variant_at = schema.fields().len() - 4;
    let last = schema.field(variant_at);
    assert_eq!(
        last.name(),
        "variant",
        "the open fields column, named as ADR-0034 names it"
    );
    assert_eq!(
        last.metadata()
            .get("ARROW:extension:name")
            .map(String::as_str),
        Some("arrow.parquet.variant"),
        "a reader that knows variants is told what this column is"
    );
    assert!(last.is_nullable(), "a row with no segment has nothing here");

    let column = batch
        .column(variant_at)
        .as_any()
        .downcast_ref::<arrow::array::StructArray>()
        .expect("the canonical variant storage type is a struct of two binaries");
    assert_eq!(column.len(), 2);
    assert_eq!(
        column.null_count(),
        1,
        "one row has a segment, one does not"
    );
    // The halves exist and the present row carries bytes in both.
    let half = |name: &str| -> &arrow::array::BinaryArray {
        column
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::BinaryArray>())
            .expect("binary half")
    };
    assert!(!half("metadata").is_empty() && !half("value").is_empty());
}

/// An open-fields segment with no row in the window is a change that touched only the
/// names the declaration does not have: it becomes a `put_open` row — declared columns
/// NULL, `variant` carrying what the log actually said. Writing the rows and skipping the
/// segment would drop truth silently; refusing the window loses the rows beside it.
#[test]
fn an_open_segment_with_no_row_is_a_put_open_row() {
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
        // A key with a segment and no row: `merge` of nothing but open names on a key
        // that was never put (a row purged in an earlier window looks the same here).
        let mut open = Dict::new();
        open.insert(
            "note".to_string(),
            okm_value::Variant::Str("orphan".to_string()),
        );
        c.put(&K2, &Variant::Map(open))
            .expect("a segment with no row");
    }

    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    assert_eq!(report.rows, 1, "one put");
    assert_eq!(report.open_fields, 1, "one segment, carried not dropped");

    let file = std::fs::File::open(&report.files[0]).expect("open");
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("reader")
        .build()
        .expect("build");
    let batch = reader.next().expect("one batch").expect("batch");
    assert_eq!(batch.num_rows(), 2, "the put and the open-fields change");
    let ops = batch
        .column_by_name("okm_op")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>())
        .expect("okm_op");
    assert_eq!((ops.value(0), ops.value(1)), ("put", "put_open"));
    let level = batch.column_by_name("level").expect("level");
    assert!(!level.is_null(0), "the put carries its declared value");
    assert!(
        level.is_null(1),
        "an open-fields change says nothing about the declared columns"
    );
    let variant = batch.column_by_name("variant").expect("variant");
    assert!(
        variant.is_valid(1),
        "and everything about the names the declaration does not have"
    );
}

/// The log is a LOG, not a rebuilt snapshot: a second window is a second version
/// holding one `add`. This is the capability the Iceberg writer had to defer
/// (snapshot append) and the Delta writer gets from the format.
#[test]
fn a_second_window_appends_one_version_with_one_add() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("out");
    let cfg = DrainConfig::new(out.path(), "test");

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
    let first = drain(root.path(), &cfg).expect("first window");

    {
        let c = collection(root.path());
        c.put(
            &K2,
            &R {
                level: 4,
                name: "beta".into(),
            },
        )
        .expect("put");
    }
    let second = drain(root.path(), &cfg).expect("second window");
    assert_ne!(first.files[0], second.files[0], "two windows, two files");

    let table_dir = second.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let log = table_dir.join("_delta_log");
    let v0 = std::fs::read_to_string(log.join(format!("{:020}.json", 0))).expect("v0");
    let v1 = std::fs::read_to_string(log.join(format!("{:020}.json", 1))).expect("v1");

    // The commit is additive: no protocol, no schema, no restatement of the first file.
    assert!(
        !v1.contains("protocol") && !v1.contains("metaData"),
        "a version carries only what changed: {v1}"
    );
    let adds: Vec<serde_json::Value> = v1
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|a| a.get("add").cloned())
        .collect();
    assert_eq!(adds.len(), 1, "one file, one add: {v1}");
    let first_name = first.files[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        !v1.contains(&first_name),
        "the file already in the log is not added again: {v1}"
    );
    assert!(
        v0.contains(&first_name),
        "and it is in the version that made it"
    );

    // Re-draining the same directory (no new window) writes nothing new.
    let third = drain(root.path(), &cfg).expect("third drain");
    assert_eq!(third.rows, 0);
    assert!(
        !log.join(format!("{:020}.json", 2)).exists(),
        "an empty window commits no version"
    );
}

/// A variant column is a table-wide promise: the log must say `variantType`, because a
/// reader that does not know the feature has to refuse the table rather than guess.
#[test]
fn a_variant_column_declares_its_table_feature() {
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
        let mut open = Dict::new();
        open.insert(
            "note".to_string(),
            okm_value::Variant::Str("first".to_string()),
        );
        c.put(&K1, &Variant::Map(open)).expect("an open field");
    }
    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    let table_dir = report.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let v0 = std::fs::read_to_string(table_dir.join("_delta_log").join(format!("{:020}.json", 0)))
        .expect("v0");
    let protocol: serde_json::Value = v0
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|a| a.get("protocol").cloned())
        .expect("a protocol action");
    assert_eq!(protocol["minReaderVersion"], 3, "{v0}");
    assert_eq!(protocol["minWriterVersion"], 7, "{v0}");
    assert_eq!(protocol["readerFeatures"][0], "variantType", "{v0}");
    // And the schema says `variant`, not the struct a reader would have to guess at.
    let schema: serde_json::Value = v0
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|a| {
            a["metaData"]["schemaString"]
                .as_str()
                .map(|s| s.to_string())
        })
        .and_then(|s| serde_json::from_str(&s).ok())
        .expect("a schema action");
    let variant = schema["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|f| f["name"] == "variant")
        .cloned()
        .expect("a variant column");
    assert_eq!(variant["type"], "variant");
}

/// A commit records what a reader can skip on: row count, per-column bounds and null
/// counts, taken from the statistics the Parquet writer already computed. Columns whose
/// physical statistics cannot state the declared type's bound (okm's unsigned keys widen
/// into a signed Delta type) carry none — an invented bound is worse than no skipping.
#[test]
fn a_commit_carries_statistics_a_reader_can_skip_with() {
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
                name: "zulu".into(),
            },
        )
        .expect("put");
    }
    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    let table_dir = report.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let text =
        std::fs::read_to_string(table_dir.join("_delta_log").join(format!("{:020}.json", 0)))
            .expect("v0");
    let stats: serde_json::Value = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|a| a["add"]["stats"].as_str().map(|s| s.to_string()))
        .and_then(|s| serde_json::from_str(&s).ok())
        .expect("an add with stats");

    assert_eq!(stats["numRecords"], 2);
    assert_eq!(stats["minValues"]["name"], "alpha");
    assert_eq!(stats["maxValues"]["name"], "zulu");
    assert_eq!(stats["nullCount"]["name"], 0);
    // The unsigned columns (`org`, `id`, `level`) have no exact Delta type, so no bound
    // is claimed for them — the file's own Parquet statistics still serve a direct reader.
    for skipped in ["org", "id", "level"] {
        assert!(
            stats["minValues"].get(skipped).is_none(),
            "no bound is invented for `{skipped}`"
        );
    }
}

/// A struct column (the `variant` half of a document) is ONE Arrow field and SEVERAL
/// Parquet leaves, so a field after it does not sit at its Arrow index. Reading the
/// metadata by index attributed the variant's bytes to `okm_op` and a null count to the
/// wrong name — and a reader then skips the file on a filter that should have matched it.
#[test]
fn statistics_name_the_right_column_when_a_struct_sits_before_it() {
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
        let mut open = Dict::new();
        open.insert(
            "note".to_string(),
            okm_value::Variant::Str("first".to_string()),
        );
        c.put(&K1, &Variant::Map(open))
            .expect("a segment beside the row");
        c.put(
            &K2,
            &R {
                level: 4,
                name: "zulu".into(),
            },
        )
        .expect("put");
    }
    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    let table_dir = report.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let text =
        std::fs::read_to_string(table_dir.join("_delta_log").join(format!("{:020}.json", 0)))
            .expect("v0");
    let stats: serde_json::Value = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|a| a["add"]["stats"].as_str().map(|s| s.to_string()))
        .and_then(|s| serde_json::from_str(&s).ok())
        .expect("an add with stats");

    // Exactly the scalar columns that HAVE a Delta-spellable bound: the two text ones.
    let mut bounds: Vec<&str> = stats["minValues"]
        .as_object()
        .expect("bounds")
        .keys()
        .map(String::as_str)
        .collect();
    bounds.sort_unstable();
    assert_eq!(
        bounds,
        ["name", "okm_op"],
        "a struct leaf must not be reported under a later column's name: {stats}"
    );
    assert_eq!(
        stats["nullCount"]["okm_op"], 0,
        "no put row is null: {stats}"
    );
    assert_eq!(stats["minValues"]["okm_op"], "put");
}

/// Folding history into current state: two windows (a put, then a put and the purge of the
/// first row) become ONE file holding the live row, and the files it replaced leave the
/// table through Delta `remove` actions rather than through change rows nobody folds
/// (ADR-0058 § Amendment — the sink half, stated as table state).
#[test]
fn a_fold_replaces_the_change_files_with_current_state() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    let cfg = DrainConfig::new(out.path(), "test");

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
    drain(root.path(), &cfg).expect("first window");

    {
        let c = collection(root.path());
        c.put(
            &K2,
            &R {
                level: 4,
                name: "beta".into(),
            },
        )
        .expect("put");
        // The store is transient, so the second window re-writes K1 before removing it —
        // which is also the sharper case: a put and the purge that follows it in one window,
        // after an earlier put of the same key in the window before.
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
    let second = drain(root.path(), &cfg).expect("second window");
    let table_dir = second.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    // Before the fold: two files, three change rows, one live key.
    let before = okm_lake::read::current_state_at(&table_dir).expect("current state");
    assert_eq!(before.num_rows(), 1, "K2 is the only live key");

    let fold = okm_lake::read::condense_at(&table_dir, 0).expect("fold");
    assert_eq!(fold.replaced, 2, "both change files");
    assert_eq!(fold.rows, 1);

    // After: one file, and the row that is left is the one that was never removed.
    let after = okm_lake::read::current_state_at(&table_dir).expect("current state");
    assert_eq!(after.num_rows(), 1);
    let id = after
        .column_by_name("id")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt64Array>())
        .expect("id column");
    assert_eq!(id.value(0), 11, "K2");
    let ops = after
        .column_by_name("okm_op")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>())
        .expect("okm_op");
    assert_eq!(ops.value(0), "put", "current state has no `del` rows left");

    // The log states the replacement: two removes, one add, in one version.
    let log = table_dir.join("_delta_log");
    let newest = std::fs::read_to_string(log.join(format!("{:020}.json", fold.version)))
        .expect("the fold's version");
    assert_eq!(
        newest.matches("\"remove\"").count(),
        2,
        "both replaced files are removed: {newest}"
    );
    assert_eq!(
        newest.matches("\"add\"").count(),
        1,
        "one file now holds the table: {newest}"
    );
    assert!(
        newest.contains("dataChange"),
        "a fold changes what a reader sees: {newest}"
    );

    // And folding again is refused rather than churning the log: one file IS current state.
    let again = okm_lake::read::condense_at(&table_dir, 0).expect_err("nothing to fold");
    assert!(again.what.contains("nothing to fold"), "got: {again}");
}

/// A fold takes files out of the TABLE and leaves the bytes in `data/`; vacuum is what
/// reclaims them, and it deletes only what no kept version can still see.
#[test]
fn a_vacuum_deletes_only_files_no_kept_version_references() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    for level in [3u32, 5] {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level,
                name: "alpha".into(),
            },
        )
        .expect("put");
        drop(c);
        drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("a window");
    }
    let mut dirs: Vec<_> = std::fs::read_dir(out.path())
        .expect("lake")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("_delta_log").is_dir())
        .collect();
    let table_dir = dirs.pop().expect("one table");
    let fold = okm_lake::read::condense_at(&table_dir, 0).expect("fold");
    assert_eq!(fold.replaced, 2);

    let entries = |dir: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(dir.join("data"))
            .expect("data")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    };
    let files = |dir: &std::path::Path| -> Vec<String> {
        entries(dir)
            .into_iter()
            .filter(|f| f.ends_with(".parquet"))
            .collect()
    };
    assert_eq!(
        files(&table_dir).len(),
        3,
        "two replaced files and the folded one: {:?}",
        entries(&table_dir)
    );

    // Keeping one version still covers the version the fold replaced: nothing goes.
    let kept = okm_lake::vacuum::vacuum_at(&table_dir, 1, false).expect("vacuum");
    assert_eq!(kept.deleted, 0, "the previous version is still readable");
    assert_eq!(files(&table_dir).len(), 3);

    // Keeping only the current version reclaims them.
    let dry = okm_lake::vacuum::vacuum_at(&table_dir, 0, true).expect("dry run");
    assert_eq!(dry.deleted, 2, "would delete both");
    assert_eq!(files(&table_dir).len(), 3, "and deleted nothing");

    let swept = okm_lake::vacuum::vacuum_at(&table_dir, 0, false).expect("vacuum");
    assert_eq!(swept.deleted, 2);
    assert!(swept.bytes > 0, "bytes are counted: {swept:?}");
    let left = entries(&table_dir);
    assert_eq!(
        left.len(),
        2,
        "the folded file and its manifest are all that is left: {left:?}"
    );
    // Nothing of the replaced files survives, manifests included.
    assert!(
        left.iter().all(|f| f.contains("-fold-")),
        "no orphan manifests: {left:?}"
    );

    // The table still answers: vacuum removes bytes, not history the log still names.
    let state = okm_lake::read::current_state_at(&table_dir).expect("current state");
    assert_eq!(state.num_rows(), 1);
    // And a second sweep has nothing to do.
    let again = okm_lake::vacuum::vacuum_at(&table_dir, 0, false).expect("vacuum again");
    assert_eq!(again.deleted, 0);
}

/// A fold is INCREMENTAL: a file whose keys are exclusive to it already states final rows,
/// so it is neither rewritten nor removed. Only the files that share a key are replaced.
#[test]
fn a_fold_leaves_files_whose_keys_are_exclusive_to_them() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");

    // Window 1: K2 alone.
    {
        let c = collection(root.path());
        c.put(
            &K2,
            &R {
                level: 4,
                name: "beta".into(),
            },
        )
        .expect("put");
    }
    drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("first window");

    // Window 2: K1, which no other file has.
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
    let second = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("second window");
    let table_dir = second.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    // Two files, no shared key: nothing to resolve, so nothing is folded.
    let refused = okm_lake::read::condense_at(&table_dir, 0).expect_err("nothing to fold");
    assert!(refused.refused, "a policy may skip this: {refused}");
    assert_eq!(
        okm_lake::read::current_state_at(&table_dir)
            .expect("state")
            .num_rows(),
        2
    );

    // Window 3: K1 again — now the two K1 files share state and must be folded into one.
    {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 9,
                name: "alpha2".into(),
            },
        )
        .expect("put");
    }
    let third = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("third window");
    assert_eq!(third.files.len(), 1);
    let k2_file = {
        let files = okm_lake::delta::files_in_order_at(&table_dir).expect("files");
        files
            .iter()
            .find(|f| f.contains("00000000-00000000"))
            .cloned()
            .expect("the K2 file")
    };

    let fold = okm_lake::read::condense_at(&table_dir, 0).expect("fold");
    assert_eq!(fold.replaced, 2, "the two files that share K1");
    assert_eq!(fold.untouched, 1, "the K2 file is already final");
    assert_eq!(fold.rows, 1, "both files held K1, which is one row");

    let live = okm_lake::delta::files_in_order_at(&table_dir).expect("files");
    assert_eq!(live.len(), 2, "the untouched file plus the folded one");
    assert!(
        live.contains(&k2_file),
        "the exclusive file was not rewritten: {live:?}"
    );
    let state = okm_lake::read::current_state_at(&table_dir).expect("state");
    assert_eq!(state.num_rows(), 2, "K1 and K2");
    let level = state
        .column_by_name("level")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>())
        .expect("level");
    let levels: Vec<u32> = (0..state.num_rows()).map(|i| level.value(i)).collect();
    assert!(levels.contains(&9), "K1 kept the newest row: {levels:?}");
    assert!(levels.contains(&4), "K2 survived: {levels:?}");
}

/// A window that holds more rows than the limit becomes several files, each with its own
/// manifest — and therefore its own, narrower key range: the limit is also what makes the
/// range index fine-grained.
#[test]
fn a_window_is_cut_into_files_by_the_row_limit() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    {
        let c = collection(root.path());
        for id in 10u64..15 {
            c.put(
                &K { org: 1, id },
                &R {
                    level: id as u32,
                    name: "row".into(),
                },
            )
            .expect("put");
        }
    }
    let mut cfg = DrainConfig::new(out.path(), "test");
    cfg.max_rows_per_file = 2;
    let report = drain(root.path(), &cfg).expect("drain");
    assert_eq!(report.files.len(), 3, "5 rows, two per file");

    let table_dir = table_of(&report.files[0]);
    let live = okm_lake::delta::files_in_order_at(&table_dir).expect("files");
    assert_eq!(live.len(), 3);
    let mut names: Vec<String> = live.to_vec();
    names.sort();
    assert!(
        names.iter().all(|rel| rel.contains("-p")),
        "the parts say they are parts: {names:?}"
    );

    // Each part bounds its own rows: the ranges differ, and none spans the whole window.
    let first = okm_lake::status::manifest_of(&table_dir.join(&names[0])).expect("a manifest");
    let last = okm_lake::status::manifest_of(&table_dir.join(&names[2])).expect("a manifest");
    assert_eq!(first.rows, 2);
    assert_eq!(last.rows, 1);
    assert_ne!(first.key_bounds, last.key_bounds, "{first:?} {last:?}");

    // The table still reads as one table.
    let state = okm_lake::read::current_state_at(&table_dir).expect("state");
    assert_eq!(state.num_rows(), 5);
}

/// A fold honours the same limit: current state does not have to land in one file either.
#[test]
fn a_fold_honours_the_row_limit() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    let mut cfg = DrainConfig::new(out.path(), "test");
    cfg.max_rows_per_file = 2;

    // Two windows, each rewriting the same three keys: every file shares a key, so the fold
    // has three rows to land and the limit says two at a time.
    let mut first_files = Vec::new();
    for level in [1u32, 2] {
        let c = collection(root.path());
        for id in 10u64..13 {
            c.put(
                &K { org: 1, id },
                &R {
                    level: level * 10 + id as u32,
                    name: "row".into(),
                },
            )
            .expect("put");
        }
        drop(c);
        let report = drain(root.path(), &cfg).expect("a window");
        if first_files.is_empty() {
            first_files = report.files.clone();
        }
    }
    let table_dir = table_of(&first_files[0]);

    let fold = okm_lake::read::condense_at(&table_dir, 2).expect("fold");
    assert_eq!(fold.rows, 3, "three live keys");
    assert_eq!(fold.files.len(), 2, "three rows, two per file");
    assert!(
        fold.files
            .iter()
            .all(|f| f.to_string_lossy().contains("-fold-")),
        "{:?}",
        fold.files
    );
    let state = okm_lake::read::current_state_at(&table_dir).expect("state");
    assert_eq!(state.num_rows(), 3);
}

/// A partitioned table writes one file per partition VALUE, and the value travels into the
/// Delta action as a string — a reader is told the partition, not left to parse a path.
#[test]
fn a_partitioned_drain_writes_one_file_per_value() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    {
        let c = collection(root.path());
        for (key, level) in [(K1, 3u32), (K3, 4)] {
            c.put(
                &key,
                &R {
                    level,
                    name: "row".into(),
                },
            )
            .expect("put");
        }
    }
    let mut cfg = DrainConfig::new(out.path(), "test");
    cfg.partition = Some("org".to_string());
    let report = drain(root.path(), &cfg).expect("drain");
    assert_eq!(report.files.len(), 2, "one file per org value");

    let table_dir = table_of(&report.files[0]);
    let mut dirs: Vec<String> = std::fs::read_dir(table_dir.join("data"))
        .expect("data")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    assert_eq!(
        dirs,
        vec!["org=1".to_string(), "org=2".to_string()],
        "one directory per partition value"
    );

    // The log states the partition: `partitionColumns` once, `partitionValues` per file.
    let adds = okm_lake::delta::live_adds_at(&table_dir).expect("adds");
    assert_eq!(adds.len(), 2);
    let values: Vec<String> = adds
        .iter()
        .map(|(path, values)| {
            assert!(
                path.starts_with("data/org="),
                "the path carries it too: {path}"
            );
            values
                .iter()
                .map(|(column, value)| format!("{column}={value}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect();
    assert!(values.contains(&"org=1".to_string()), "{values:?}");
    assert!(values.contains(&"org=2".to_string()), "{values:?}");

    // And the table reads as one table: two rows, from two directories.
    let state = okm_lake::read::current_state_at(&table_dir).expect("state");
    assert_eq!(state.num_rows(), 2);
}

/// A fold stays inside its partition: a key lives in one partition because the partition
/// column is a key column, so replacing a key's file cannot involve another partition.
#[test]
fn a_fold_stays_inside_a_partition() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    let mut cfg = DrainConfig::new(out.path(), "test");
    cfg.partition = Some("org".to_string());

    {
        let c = collection(root.path());
        for (key, level) in [(K1, 3u32), (K3, 4)] {
            c.put(
                &key,
                &R {
                    level,
                    name: "row".into(),
                },
            )
            .expect("put");
        }
    }
    let first = drain(root.path(), &cfg).expect("first window");
    let table_dir = table_of(&first.files[0]);

    {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 9,
                name: "row2".into(),
            },
        )
        .expect("put");
    }
    drain(root.path(), &cfg).expect("second window");

    let fold = okm_lake::read::condense_at(&table_dir, 0).expect("fold");
    assert_eq!(fold.replaced, 2, "org 1's two windows");
    assert_eq!(fold.untouched, 1, "org 2 is another partition");
    assert_eq!(
        fold.pruned, 1,
        "and the range excluded it: org 2 is outside org 1's range"
    );
    assert_eq!(fold.files.len(), 1, "one folded file, in org=1");
    assert!(
        fold.files[0].to_string_lossy().contains("/org=1/"),
        "and it went back into its own partition dir: {:?}",
        fold.files[0]
    );

    let state = okm_lake::read::current_state_at(&table_dir).expect("state");
    assert_eq!(state.num_rows(), 2, "org 1 and org 2");
    let levels: Vec<u32> = {
        let column = state
            .column_by_name("level")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>())
            .expect("level");
        (0..state.num_rows()).map(|i| column.value(i)).collect()
    };
    assert!(levels.contains(&9), "org 1's newest row: {levels:?}");
    assert!(levels.contains(&4), "org 2 untouched: {levels:?}");
}

/// A partition has to refine the key space: partitioning by a payload column would let one
/// key appear in two partitions, where a fold cannot see both halves of it.
#[test]
fn a_partition_must_be_a_key_column() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 3,
                name: "row".into(),
            },
        )
        .expect("put");
    }
    let mut cfg = DrainConfig::new(out.path(), "test");
    cfg.partition = Some("level".to_string());
    let err = drain(root.path(), &cfg).expect_err("level is not a key");
    assert!(
        err.what.contains("not a key column"),
        "the refusal says why: {err}"
    );
    // Nothing landed: a refused window leaves no file and no moved position.
    assert!(
        !out.path().join("0047/data").exists(),
        "no half-committed directory"
    );
}

/// The manifest records the range a file's keys cover, and a fold uses it to skip files that
/// cannot share a key — so a key written twice is resolved against the file that holds it, not
/// against the whole table.
#[test]
fn the_key_range_in_a_manifest_prunes_the_fold() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");

    // Window 1 writes org 1, window 2 writes org 2: the ranges cannot overlap.
    for (key, level) in [(K1, 3u32), (K2, 4)] {
        let c = collection(root.path());
        c.put(
            &key,
            &R {
                level,
                name: "row".into(),
            },
        )
        .expect("put");
        drop(c);
        drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("a window");
    }
    let mut dirs: Vec<_> = std::fs::read_dir(out.path())
        .expect("lake")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("_delta_log").is_dir())
        .collect();
    let table_dir = dirs.pop().expect("one table");

    // Both windows recorded bounds, and the ranges do not overlap on `org`.
    let live = okm_lake::delta::files_in_order_at(&table_dir).expect("files");
    assert_eq!(live.len(), 2);
    let bounds: Vec<_> = live
        .iter()
        .map(|rel| {
            let manifest = okm_lake::status::manifest_of(&table_dir.join(rel)).expect("a manifest");
            assert!(
                !manifest.key_bounds.is_empty(),
                "a drained file records where its keys start and end: {manifest:?}"
            );
            okm_lake::bounds::from_manifest(&manifest.key_bounds)
        })
        .collect();
    assert!(
        !bounds[0].overlaps(&bounds[1]),
        "org 1 and org 2 cannot share a key: {bounds:?}"
    );

    // Window 3 rewrites org 1's key: only the file that could hold it is folded.
    {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 9,
                name: "row2".into(),
            },
        )
        .expect("put");
    }
    drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("third window");

    let fold = okm_lake::read::condense_at(&table_dir, 0).expect("fold");
    assert_eq!(fold.replaced, 2, "org 1's two files");
    assert_eq!(fold.untouched, 1, "org 2's file is out of range");
    assert_eq!(fold.rows, 1);

    let after = okm_lake::delta::files_in_order_at(&table_dir).expect("files");
    assert_eq!(after.len(), 2, "the untouched file plus the folded one");
    let state = okm_lake::read::current_state_at(&table_dir).expect("state");
    assert_eq!(state.num_rows(), 2);
    let levels: Vec<u32> = {
        let column = state
            .column_by_name("level")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>())
            .expect("level");
        (0..state.num_rows()).map(|i| column.value(i)).collect()
    };
    assert!(levels.contains(&9), "org 1's newest row: {levels:?}");
    assert!(levels.contains(&4), "org 2 untouched: {levels:?}");
}

/// `status` answers an operator's questions from the log and the manifests: how many files,
/// how many bytes, how many change rows, is it folded, and what would a vacuum reclaim.
#[test]
fn a_status_reads_the_lake_without_opening_a_payload() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    for level in [3u32, 5] {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level,
                name: "alpha".into(),
            },
        )
        .expect("put");
        drop(c);
        drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("a window");
    }
    let mut dirs: Vec<_> = std::fs::read_dir(out.path())
        .expect("lake")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("_delta_log").is_dir())
        .collect();
    let table_dir = dirs.pop().expect("one table");

    let before = okm_lake::status::status_at(&table_dir, false).expect("status");
    assert_eq!(before.live_files, 2);
    assert_eq!(before.change_rows, Some(2), "one row per window");
    assert_eq!(before.deletes, Some(0));
    assert!(!before.folded);
    assert_eq!(before.stale_files, 0);
    assert_eq!(before.live_rows, None, "not asked, so not folded");

    okm_lake::read::condense_at(&table_dir, 0).expect("fold");
    let folded = okm_lake::status::status_at(&table_dir, true).expect("status");
    assert_eq!(folded.live_files, 1);
    assert!(folded.folded, "the newest file is a fold");
    assert_eq!(
        folded.change_rows,
        Some(1),
        "and it has a manifest of its own"
    );
    assert_eq!(folded.live_rows, Some(1));
    assert_eq!(
        folded.stale_files, 2,
        "the replaced files are still on disk"
    );
    assert!(folded.stale_bytes > 0);

    okm_lake::vacuum::vacuum_at(&table_dir, 0, false).expect("vacuum");
    let swept = okm_lake::status::status_at(&table_dir, false).expect("status");
    assert_eq!(swept.stale_files, 0, "nothing left to reclaim");
    assert_eq!(swept.live_files, 1);
}

/// One window can hold a removal, and then ONE file is worth folding: a single file with one
/// op per key already is current state, but one that carries `del` rows or a key written
/// twice is not.
#[test]
fn a_single_file_with_removals_is_foldable() {
    let root = tempfile::tempdir().expect("root");
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
        assert!(c.purge(&K1).expect("purge"), "the row existed");
    }
    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    assert_eq!(report.files.len(), 1, "one window, one file");
    let table_dir = report.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let fold =
        okm_lake::read::condense_at(&table_dir, 0).expect("three change rows are worth folding");
    assert_eq!(fold.replaced, 1);
    assert_eq!(fold.rows, 1, "K2 is the only live key");

    let after = okm_lake::read::current_state_at(&table_dir).expect("current state");
    assert_eq!(after.num_rows(), 1);
    // And with one op per key, the same file is left alone next time.
    let again = okm_lake::read::condense_at(&table_dir, 0).expect_err("nothing left to fold");
    assert!(again.what.contains("nothing to fold"), "got: {again}");
}

/// A drain can keep the lake current on its own: the policy fires once the change files pile
/// up, and a table below the threshold is left exactly as it was.
#[test]
fn a_drain_folds_the_table_when_the_policy_says_so() {
    let root = tempfile::tempdir().expect("root");
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
    }
    let first = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("first window");
    assert_eq!(first.folds, 0, "no policy, no fold");

    {
        let c = collection(root.path());
        c.put(
            &K2,
            &R {
                level: 4,
                name: "beta".into(),
            },
        )
        .expect("put");
        c.put(
            &K1,
            &R {
                level: 5,
                name: "alpha2".into(),
            },
        )
        .expect("the key the first window wrote, so the two files share state");
    }
    // Two files is the threshold: this drain writes the second and folds both.
    let mut cfg = DrainConfig::new(out.path(), "test");
    cfg.fold = Some(okm_lake::FoldPolicy::files(2));
    let second = drain(root.path(), &cfg).expect("second window");
    assert_eq!(second.folds, 1, "the policy fired");

    let table_dir = second.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    // The LOG is the truth: a replaced file leaves the table through a `remove` action, and
    // the bytes stay on disk for time travel (dropping those is a vacuum, which this writer
    // does not have yet).
    let live = okm_lake::delta::files_in_order_at(&table_dir).expect("the log's files");
    assert_eq!(live.len(), 1, "one current-state file: {live:?}");
    assert!(
        live[0].contains("-fold-"),
        "and it is the folded one: {live:?}"
    );
    let state = okm_lake::read::current_state_at(&table_dir).expect("current state");
    assert_eq!(state.num_rows(), 2, "both keys are live");

    // A threshold above the current size does nothing.
    let mut high = DrainConfig::new(out.path(), "test");
    high.fold = Some(okm_lake::FoldPolicy::files(9));
    let third = drain(root.path(), &high).expect("nothing new to drain");
    assert_eq!(third.folds, 0);
}

/// A fold keeps BOTH halves of a document: the declared columns from the window that last
/// wrote a row, and the open fields from the window that last wrote those — a `put` in a
/// later window does not erase the open half of the row before it.
#[test]
fn a_fold_keeps_open_fields_written_by_an_earlier_window() {
    let root = tempfile::tempdir().expect("root");
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
        let mut open = Dict::new();
        open.insert(
            "note".to_string(),
            okm_value::Variant::Str("first".to_string()),
        );
        c.put(&K1, &Variant::Map(open)).expect("an open field");
    }
    drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("first window");

    {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 7,
                name: "alpha2".into(),
            },
        )
        .expect("put");
    }
    let second = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("second window");
    let table_dir = second.files[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let fold = okm_lake::read::condense_at(&table_dir, 0).expect("fold");
    assert_eq!(fold.replaced, 2);
    assert_eq!(fold.rows, 1);

    let state = okm_lake::read::current_state_at(&table_dir).expect("current state");
    assert_eq!(state.num_rows(), 1);
    let level = state
        .column_by_name("level")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt32Array>())
        .expect("level");
    assert_eq!(level.value(0), 7, "the declared half is the newest row");
    let variant = state
        .column_by_name("variant")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::StructArray>())
        .expect("variant");
    assert!(
        !variant.is_null(0),
        "the open half came from the earlier window instead of being dropped"
    );
    let ops = state
        .column_by_name("okm_op")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>())
        .expect("okm_op");
    assert_eq!(
        ops.value(0),
        "put",
        "a folded row states a row, not a change"
    );
}

/// Two windows of open fields for one key MERGE: the fold decodes both variant cells, unions
/// them field by field (the later window winning where they overlap) and re-encodes — so a
/// document's open half accumulates instead of the newer cell replacing the older.
#[test]
fn a_fold_merges_open_fields_written_by_two_windows() {
    let root = tempfile::tempdir().expect("root");
    let out = tempfile::tempdir().expect("lake");
    for (field, value) in [("note", "first"), ("tag", "second")] {
        let c = collection(root.path());
        c.put(
            &K1,
            &R {
                level: 3,
                name: "alpha".into(),
            },
        )
        .expect("put");
        let mut open = Dict::new();
        open.insert(
            field.to_string(),
            okm_value::Variant::Str(value.to_string()),
        );
        c.put(&K1, &Variant::Map(open)).expect("an open field");
        drop(c);
        drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("a window");
    }

    let mut dirs: Vec<_> = std::fs::read_dir(out.path())
        .expect("lake")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("_delta_log").is_dir())
        .collect();
    let table_dir = dirs.pop().expect("one table");

    let fold =
        okm_lake::read::condense_at(&table_dir, 0).expect("both windows written open fields");
    assert_eq!(fold.replaced, 2, "the two windows that touched K1");
    assert_eq!(fold.rows, 1);

    // One row, and its variant cell holds BOTH fields.
    let state = okm_lake::read::current_state_at(&table_dir).expect("current state");
    assert_eq!(state.num_rows(), 1);
    let variant = state
        .column_by_name("variant")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::StructArray>())
        .expect("variant");
    assert!(!variant.is_null(0), "the merged open half is there");
    let cells = okm_lake::read::open_fields(variant, 0).expect("decode");
    assert_eq!(
        cells.get("note").map(|v| format!("{v:?}")),
        Some("Str(\"first\")".to_string()),
        "the earlier window's field survived"
    );
    assert_eq!(
        cells.get("tag").map(|v| format!("{v:?}")),
        Some("Str(\"second\")".to_string()),
        "and the later one is there too"
    );
}

/// A second generation of the SAME table (same ns, new number): the directory outlives the
/// build, so a drain that stops at one generation would advance the position and report
/// success while leaving the newer rows unexported.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_shared_ns]
#[ok_ref(K)]
#[ok_ns(71)]
#[ok_layout(version = 2)]
#[ok_name("drained_rows")]
pub struct R2 {
    pub level: u32,
    pub name: String,
}

#[test]
fn one_drain_covers_every_generation_in_the_directory() {
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
    {
        let c: Collection<_, K, R2> = Collection::captured(ChangelogStore::new(
            TestStore::slatedb_mem(),
            ChangelogConfig::new(root.path()),
        ))
        .expect("the next build opens the same directory");
        c.put(
            &K2,
            &R2 {
                level: 4,
                name: "beta".into(),
            },
        )
        .expect("put");
    }

    let report = drain(root.path(), &DrainConfig::new(out.path(), "test")).expect("drain");
    assert_eq!(
        report.files.len(),
        2,
        "one window per generation, both taken in one pass: {:?}",
        report.files
    );
    assert_eq!(report.rows, 2, "a row from each generation");
}
