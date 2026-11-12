//! A table a publisher WROTE is a table a reader can open.
//!
//! `publish` states a table's current state directly: rows, no ops, one file per version. The
//! claims under test are the reader's side of that bargain — the rows are the answer, a fold has
//! nothing to do and says so, `status` knows how many rows there are without opening the file,
//! and a publisher holding several partitions can state all of them in ONE version.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use arrow::array::{Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use okm_lake::publish::{PublishedFile, replace_files, replace_table};
use okm_lake::store::{LakeStore, LocalFs};
use okm_lake::{delta, read, status};

/// A final-state Parquet file: the table's columns and nothing else — no `okm_op`, because
/// there is no history to state.
fn parquet(ids: &[i64]) -> Vec<u8> {
    let schema = std::sync::Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
    let batch = RecordBatch::try_new(
        schema,
        vec![std::sync::Arc::new(Int64Array::from(ids.to_vec()))],
    )
    .unwrap();
    okm_lake::snapshot::parquet_io::batch_bytes(&batch).unwrap()
}

/// A partitioned final-state file: the key the table splits on (`org`, a string so a value can
/// need encoding) beside a column, and no `okm_op` — a published file states final rows.
fn partitioned_parquet(org: &str, ids: &[i64]) -> Vec<u8> {
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("org", DataType::Utf8, false),
        Field::new("id", DataType::Int64, false),
    ]));
    let count = ids.len();
    let batch = RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow::array::StringArray::from(vec![org; count])),
            std::sync::Arc::new(Int64Array::from(ids.to_vec())),
        ],
    )
    .unwrap();
    okm_lake::snapshot::parquet_io::batch_bytes(&batch).unwrap()
}

/// Publish `versions` into the lake at `root`, one file per version, and answer the lake and
/// the table's name in it.
fn published(root: &std::path::Path, versions: &[&[i64]]) -> (LocalFs, String) {
    let lake = LocalFs::new(root);
    let table = "pub".to_string();
    let keys = vec!["id".to_string()];
    for (round, ids) in versions.iter().enumerate() {
        replace_table(
            &lake,
            &table,
            &format!("pub-{round}.parquet"),
            &parquet(ids),
            &keys,
        )
        .unwrap();
    }
    (lake, table)
}

fn ids(batch: &RecordBatch) -> Vec<i64> {
    let column = batch
        .column_by_name("id")
        .expect("the published column")
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("an Int64 column");
    (0..column.len()).map(|row| column.value(row)).collect()
}

#[test]
fn a_published_table_reads_back_as_its_newest_version() {
    let dir = tempfile::tempdir().unwrap();
    let (lake, table) = published(dir.path(), &[&[1, 2, 3], &[1, 2, 3, 4]]);

    // A published file states FINAL rows: no `okm_op` column, so nothing is folded — the rows
    // of the newest version ARE the answer.
    let state = read::current_state(&lake, &table).expect("a published table reads");
    assert_eq!(ids(&state), vec![1, 2, 3, 4], "the latest version's rows");
    assert!(
        state.column_by_name("okm_op").is_none(),
        "a final-row read does not invent an op column"
    );

    // One live file, and it is the second publish's.
    assert_eq!(
        delta::files_in_order(&lake, &table).unwrap(),
        vec!["data/pub-1.parquet".to_string()]
    );
}

#[test]
fn a_publish_states_its_row_count_and_key_range() {
    let dir = tempfile::tempdir().unwrap();
    let (lake, table) = published(dir.path(), &[&[10, 20, 30]]);

    // `status` reads the manifest instead of the file: one published file without one would make
    // the sum "unknown" rather than partial.
    let report = status::status(&lake, &table, false).expect("status");
    assert_eq!(
        report.change_rows,
        Some(3),
        "the count is known, not unknown"
    );
    assert_eq!(report.deletes, Some(0), "a final file removes nothing");
    assert!(!report.folded, "a published file is not a fold");

    let live = delta::files_in_order(&lake, &table).unwrap();
    let manifest = status::manifest_at(&lake, &table, &live[0]).expect("a published manifest");
    assert_eq!(manifest.rows, 3);
    assert!(
        manifest.segments.is_empty(),
        "a published file projects no window"
    );
    // The range the keys cover, in the manifest's sortable-byte hex (big-endian with the sign bit
    // flipped for a signed integer): this is the index a fold prunes with.
    assert_eq!(
        manifest.key_bounds,
        vec![(
            "id".to_string(),
            "800000000000000a".to_string(),
            "800000000000001e".to_string()
        )]
    );

    // The Delta action states the same thing to a reader that never sees okm's manifest.
    let version = std::fs::read_to_string(
        lake.local_root()
            .expect("a directory lake")
            .join(&table)
            .join("_delta_log/00000000000000000000.json"),
    )
    .expect("the first version");
    let add = version
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("an action"))
        .find(|action| action.get("add").is_some())
        .expect("an add action")
        .clone();
    let stats: serde_json::Value =
        serde_json::from_str(add["add"]["stats"].as_str().expect("stats is a string"))
            .expect("stats is JSON");
    assert_eq!(stats["numRecords"], 3, "the row count in the log: {stats}");
    assert_eq!(
        stats["minValues"]["id"], 10,
        "the range in the log: {stats}"
    );
    assert_eq!(
        stats["maxValues"]["id"], 30,
        "the range in the log: {stats}"
    );

    // Publishing again leaves ONE manifest: the one the new file earned.
    let (lake, table) = published(dir.path(), &[&[10, 20, 30], &[10, 20]]);
    let live = delta::files_in_order(&lake, &table).unwrap();
    assert_eq!(live.len(), 1);
    assert!(
        lake.head(&format!("{table}/data/pub-0.manifest.json"))
            .unwrap()
            .is_none(),
        "the replaced file's manifest goes with the file"
    );
    assert_eq!(
        status::status(&lake, &table, false).unwrap().change_rows,
        Some(2)
    );
}

#[test]
fn a_published_table_has_nothing_to_fold() {
    let dir = tempfile::tempdir().unwrap();
    let (lake, table) = published(dir.path(), &[&[1, 2], &[1, 2, 3]]);

    // Every key lives in one file and that file removes nothing, so a fold would churn the log
    // for an answer the table already states. Refused, and refused as a POLICY answer: a drain
    // that folds on a threshold skips it instead of failing.
    let refused = read::condense(&lake, &table).expect_err("nothing to fold");
    assert!(
        refused.refused,
        "a table that already is current: {refused}"
    );
    assert!(
        refused.what.contains("nothing to fold"),
        "the refusal names the reason: {refused}"
    );

    // And the rows are still readable afterwards: refusing changed nothing.
    let state = read::current_state(&lake, &table).unwrap();
    assert_eq!(ids(&state), vec![1, 2, 3]);
}

/// The two halves of a published partitioned table, as one version.
///
/// A publisher that holds partitions holds all of them NOW. Stating them one version at a time
/// would leave a window in which one partition answers this interval and the other answers the
/// last one — a table that is never wrong and never right either.
#[test]
fn one_version_can_hold_a_file_per_partition() {
    let dir = tempfile::tempdir().unwrap();
    let lake = LocalFs::new(dir.path());
    let table = "parts";
    let keys = vec!["org".to_string(), "id".to_string()];

    let first_org = partitioned_parquet("1", &[10, 11]);
    let second_org = partitioned_parquet("2", &[20]);
    let published = replace_files(
        &lake,
        table,
        &[
            PublishedFile::partitioned("parts-0.parquet", &first_org, "org", "1"),
            PublishedFile::partitioned("parts-0.parquet", &second_org, "org", "2"),
        ],
        &keys,
    )
    .expect("a partitioned publish");

    // ONE version names both files, and each landed in its own partition's directory.
    assert_eq!(published.commit.version, 0);
    assert_eq!(published.replaced, 0);
    assert_eq!(
        published.files,
        vec![
            "data/org=1/parts-0.parquet".to_string(),
            "data/org=2/parts-0.parquet".to_string(),
        ]
    );
    assert_eq!(
        published.bytes,
        (first_org.len() + second_org.len()) as u64,
        "the version's bytes are the sum of its files"
    );
    for file in &published.files {
        assert!(
            lake.head(&format!("{table}/{file}")).unwrap().is_some(),
            "the file is in the store: {file}"
        );
        assert!(
            status::manifest_at(&lake, table, file).is_some(),
            "a manifest beside every file: {file}"
        );
    }

    // The log states the partition as the RAW text; only the directory is encoded.
    let adds = delta::live_adds(&lake, table).unwrap();
    assert_eq!(
        adds,
        vec![
            (
                "data/org=1/parts-0.parquet".to_string(),
                vec![("org".to_string(), "1".to_string())]
            ),
            (
                "data/org=2/parts-0.parquet".to_string(),
                vec![("org".to_string(), "2".to_string())]
            ),
        ]
    );

    // A reader of that version sees the union: one table, two files.
    let state = read::current_state(&lake, table).expect("both partitions read");
    let mut got = ids(&state);
    got.sort_unstable();
    assert_eq!(got, vec![10, 11, 20]);

    // And `status` counts the version, not a file: two live files, three rows, no removals.
    let report = status::status(&lake, table, true).expect("status");
    assert_eq!(report.live_files, 2, "one file per partition value");
    assert_eq!(report.change_rows, Some(3), "summed over the files");
    assert_eq!(report.deletes, Some(0));
    assert_eq!(report.live_rows, Some(3), "and the rows fold to themselves");
    assert!(!report.folded);

    // The next version replaces BOTH partitions in one commit. A partition value that needs
    // encoding proves the split: the directory is percent-encoded, the action is not.
    let first_again = partitioned_parquet("acme/1", &[10]);
    let second_again = partitioned_parquet("2", &[20, 21]);
    let published = replace_files(
        &lake,
        table,
        &[
            PublishedFile::partitioned("parts-1.parquet", &first_again, "org", "acme/1"),
            PublishedFile::partitioned("parts-1.parquet", &second_again, "org", "2"),
        ],
        &keys,
    )
    .expect("a second publish");

    assert_eq!(published.commit.version, 1);
    assert_eq!(published.replaced, 2, "every file of the old version left");
    assert_eq!(published.files[0], "data/org=acme%2F1/parts-1.parquet");
    for gone in ["data/org=1/parts-0.parquet", "data/org=2/parts-0.parquet"] {
        assert!(
            lake.head(&format!("{table}/{gone}")).unwrap().is_none(),
            "the replaced partition left the store: {gone}"
        );
        assert!(
            lake.head(&format!("{table}/{}", status::manifest_key(gone)))
                .unwrap()
                .is_none(),
            "and its manifest went with it: {gone}"
        );
    }
    let mut got = ids(&read::current_state(&lake, table).expect("the new version reads"));
    got.sort_unstable();
    assert_eq!(
        got,
        vec![10, 20, 21],
        "both partitions state the new version"
    );
    assert_eq!(
        delta::live_adds(&lake, table).unwrap()[0].1,
        vec![("org".to_string(), "acme/1".to_string())],
        "the action keeps the raw value"
    );
}

/// The partition rules a published version cannot break: a partition is part of the KEY space,
/// and one file states one value.
#[test]
fn a_published_partition_has_to_refine_the_key_space() {
    let dir = tempfile::tempdir().unwrap();
    let lake = LocalFs::new(dir.path());
    let table = "rules";
    // `id` is a column of the file, but this table's key is `org` alone — which is what
    // makes it the wrong thing to partition by.
    let keys = vec!["org".to_string()];
    let bytes = partitioned_parquet("1", &[1]);

    // `id` is a column, not a key: partitioning by it would let one key live in two files,
    // where a fold can no longer see both halves of it.
    let refused = replace_files(
        &lake,
        table,
        &[PublishedFile::partitioned(
            "rules-0.parquet",
            &bytes,
            "id",
            "1",
        )],
        &keys,
    )
    .expect_err("a partition that is not a key");
    assert!(
        refused.contains("is not a key column"),
        "the refusal names the rule: {refused}"
    );

    // Two files for one value is one key in two places.
    let refused = replace_files(
        &lake,
        table,
        &[
            PublishedFile::partitioned("rules-0.parquet", &bytes, "org", "1"),
            PublishedFile::partitioned("rules-1.parquet", &bytes, "org", "1"),
        ],
        &keys,
    )
    .expect_err("two files, one value");
    assert!(
        refused.contains("one file per partition value"),
        "the refusal names the rule: {refused}"
    );

    // A refused publish writes nothing: no file, no manifest, no version.
    assert!(
        delta::files_in_order(&lake, table).unwrap().is_empty(),
        "a refused version leaves no file"
    );
    assert_eq!(delta::next_version(&lake, table).unwrap(), 0);
}
