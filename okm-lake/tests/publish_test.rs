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
use okm_lake::publish::replace_table;
use okm_lake::store::LocalFs;
use okm_lake::{delta, read};

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
