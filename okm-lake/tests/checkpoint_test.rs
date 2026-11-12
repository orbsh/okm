//! A checkpoint is worth writing only if reading from it answers what a full replay answers.
//!
//! The claim under test is not "a file appeared": it is that a reader which starts at the
//! checkpoint — with the versions behind it GONE — reports the same state as a reader that
//! replayed from version 0. Both shapes are covered: a publisher's (one current-state file per
//! version, `okm-lake::publish`) and a change table's (`read::current_state` folding rows).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::{Collection, DocumentEncode, KeyEncode};
use okm_lake::publish::{PublishOptions, Published, replace_table, replace_table_with};
use okm_lake::store::{LakeStore, LocalFs};
use okm_lake::{DrainConfig, delta, drain, read};

fn parquet(rows: i64) -> Vec<u8> {
    use arrow::array::{Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    let schema = std::sync::Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
    let batch = RecordBatch::try_new(
        schema,
        vec![std::sync::Arc::new(Int64Array::from(vec![
            1i64;
            rows as usize
        ]))],
    )
    .unwrap();
    okm_lake::snapshot::parquet_io::batch_bytes(&batch).unwrap()
}

/// The rows a lake's live files hold, read through the store — the state a reader sees.
fn live_rows(lake: &dyn LakeStore, table: &str) -> (Vec<String>, u64) {
    let files = delta::files_in_order(lake, table).expect("the log reads");
    let mut rows = 0u64;
    for rel in &files {
        let bytes = lake
            .get(&format!("{table}/{rel}"))
            .expect("a live file is in the store");
        rows += delta::file_summary(&bytes).expect("a Parquet summary").rows;
    }
    (files, rows)
}

/// Publish `versions` snapshots, checkpointing every `every` versions (`0` = never).
fn publish_many(lake: &dyn LakeStore, table: &str, versions: i64, every: usize) -> Vec<Published> {
    let keys = vec!["id".to_string()];
    let options = PublishOptions::every(every);
    (0..versions)
        .map(|round| {
            let bytes = parquet(round + 1);
            replace_table_with(
                lake,
                table,
                &format!("part-{round}.parquet"),
                &bytes,
                &keys,
                &options,
            )
            .expect("publish")
        })
        .collect()
}

#[test]
fn a_publisher_reader_starts_at_the_checkpoint_and_sees_the_same_state() {
    let dir = tempfile::tempdir().expect("lake");
    let lake = LocalFs::new(dir.path());

    // The same seven snapshots, one table checkpointed every third version and one not.
    let with = publish_many(&lake, "with", 7, 3);
    publish_many(&lake, "plain", 7, 0);
    assert_eq!(
        with.iter().map(|p| p.checkpoint).collect::<Vec<_>>(),
        vec![None, None, Some(2), None, None, Some(5), None],
        "a checkpoint lands on the interval, not on every version"
    );
    assert_eq!(
        delta::checkpoint_at(&lake, "plain").unwrap(),
        None,
        "a publisher that asked for none has none"
    );

    let before = live_rows(&lake, "with");
    // One live file either way: the checkpoint collapses the LOG, not the table.
    assert_eq!(delta::files_in_order(&lake, "with").unwrap().len(), 1);
    assert_eq!(before, live_rows(&lake, "plain"));

    // Reclaim what the checkpoint makes unnecessary: every version up to and including it.
    for version in 0..=5u64 {
        lake.delete(&format!("with/_delta_log/{version:020}.json"))
            .expect("delete an old version");
    }
    assert_eq!(
        delta::checkpoint_at(&lake, "with").unwrap(),
        Some(5),
        "reading starts at the checkpoint"
    );

    // The reader is left with the checkpoint plus version 6 — and the same answer.
    let after = live_rows(&lake, "with");
    assert_eq!(after, before, "same live file, same rows");
    assert_eq!(after.1, 7, "the newest snapshot is seven rows");
}

// ---- the same claim on a change table: a fold must agree, not just a file list ----

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(93)]
#[ok_name("checkpoint_rows")]
pub struct R {
    pub level: u32,
}

fn collection(root: &std::path::Path) -> Collection<ChangelogStore<TestStore>, K, R> {
    Collection::captured(ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root),
    ))
    .expect("the log opens")
}

fn write(root: &std::path::Path, id: u64, level: u32) {
    let c = collection(root);
    c.put(&K { id }, &R { level }).expect("put");
    // Drop seals the segment: a clean exit is what makes the window drainable.
}

#[test]
fn a_change_tables_fold_agrees_across_a_checkpoint() {
    let log = tempfile::tempdir().expect("log");
    let lake_dir = tempfile::tempdir().expect("lake");
    let cfg = DrainConfig::new(lake_dir.path(), "test");

    // Three windows, so the table has versions 0..=2 to potentially have to replay.
    write(log.path(), 1, 1);
    let first = drain(log.path(), &cfg).expect("window 1");
    write(log.path(), 2, 2);
    drain(log.path(), &cfg).expect("window 2");
    write(log.path(), 1, 3);
    drain(log.path(), &cfg).expect("window 3");

    let table_dir = first.files[0]
        .parent()
        .expect("data/")
        .parent()
        .expect("the table directory")
        .to_path_buf();
    let lake = LocalFs::new(table_dir.parent().expect("the lake root"));
    let table = table_dir
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .to_string();

    let replayed = read::current_state_at(&table_dir).expect("a full replay");
    assert_eq!(replayed.num_rows(), 2, "two keys, the later put wins");

    let checkpoint = delta::write_checkpoint(&lake, &table).expect("a checkpoint");
    assert_eq!(checkpoint.version, 2, "the newest version");
    assert_eq!(
        checkpoint.files, 3,
        "three change files are the table's state"
    );

    for version in 0..2u64 {
        lake.delete(&format!("{table}/_delta_log/{version:020}.json"))
            .expect("delete an old version");
    }
    // Nothing but the checkpoint and the version it states: a replay from 0 is impossible
    // and unnecessary.
    assert_eq!(delta::checkpoint_at(&lake, &table).unwrap(), Some(2));
    assert!(
        !lake
            .head(&format!("{table}/_delta_log/{:020}.json", 0))
            .unwrap()
            .is_some()
    );

    let from_checkpoint = read::current_state_at(&table_dir).expect("a read from the checkpoint");
    assert_eq!(
        from_checkpoint, replayed,
        "the fold answers the same rows, in the same order"
    );
    // ...and the key columns still come from the log, which the checkpoint carries too.
    assert_eq!(
        read::key_columns_at(&table_dir).expect("key columns"),
        vec!["id".to_string()]
    );

    // A publish and a checkpoint at the same version never fight over the same key.
    let published = replace_table(
        &lake,
        "single",
        "single.parquet",
        &parquet(1),
        &["id".into()],
    )
    .expect("a table of one file");
    assert_eq!(published.commit.version, 0);
}
