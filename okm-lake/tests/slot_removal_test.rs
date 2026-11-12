//! A removal can name ONE FACE, and a reader has to resolve it that way.
//!
//! `okm_slot = 1` is a document's open half. It is drainable (a full-length suffix makes it
//! deletable), and ONE write path emits it alone: a whole-document put whose map carries only
//! declared names — `Collection::put(&k, (&Variant::Map(obj),))` → `replace_map` →
//! `slot1_into`, which DELETES the dynamic entry instead of writing a zero-frame list. The row it
//! belongs to is written by the same commit, so a reader that treats the `del` as "the key is
//! gone" drops a row that still exists.
//!
//! This suite drives that path through the real engine, the real changelog and the real drain —
//! not a hand-built change file — so what it proves is what a sink actually receives.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use arrow::array::{Array, Int64Array, StringArray, UInt16Array};
use okm_async::TestStore;
use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::{Collection, DocumentEncode, KeyEncode};
use okm_lake::store::{LakeStore, LocalFs};
use okm_lake::{DrainConfig, drain, read};
use okm_value::{Dict, Variant};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(95)]
#[ok_name("slot_rows")]
pub struct R {
    pub level: u32,
    pub name: String,
}

const K1: K = K { id: 1 };
const K2: K = K { id: 2 };

/// A handle over the SAME keyspace: clones share the state, so a second window sees what the
/// first one wrote — which is what makes `purge` (and `replace`) a read-modify-write of a real
/// document instead of a blind write.
fn collection(
    store: &TestStore,
    root: &std::path::Path,
) -> Collection<ChangelogStore<TestStore>, K, R> {
    Collection::captured(ChangelogStore::new(
        store.clone(),
        ChangelogConfig::new(root),
    ))
    .expect("the log opens")
}

/// A map of declared names only: the shape whose dynamic half is empty.
fn declared_only(level: u32, name: &str) -> Dict {
    let mut map = Dict::new();
    map.insert("level".to_string(), Variant::U64(level as u64));
    map.insert("name".to_string(), Variant::Str(name.to_string()));
    map
}

fn open_field(name: &str, value: &str) -> Dict {
    let mut map = Dict::new();
    map.insert(name.to_string(), Variant::Str(value.to_string()));
    map
}

/// Every `(okm_op, okm_slot)` pair in one change file, in file order.
fn ops_of(file: &std::path::Path) -> Vec<(String, Option<u16>)> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(file).expect("open"))
        .expect("reader")
        .build()
        .expect("build");
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.expect("batch");
        let ops = batch
            .column_by_name("okm_op")
            .expect("a change file carries ops")
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("an op column");
        let slots = batch
            .column_by_name("okm_slot")
            .and_then(|c| c.as_any().downcast_ref::<UInt16Array>());
        for row in 0..batch.num_rows() {
            let slot = match slots {
                Some(slot) if !slot.is_null(row) => Some(slot.value(row)),
                _ => None,
            };
            out.push((ops.value(row).to_string(), slot));
        }
    }
    out
}

/// The table directory a drained file belongs to.
fn table_of(file: &std::path::Path) -> std::path::PathBuf {
    file.parent()
        .expect("data/")
        .parent()
        .expect("the table directory")
        .to_path_buf()
}

#[test]
fn a_whole_document_replacement_drains_as_a_slot_one_removal() {
    let log = tempfile::tempdir().expect("log");
    let out = tempfile::tempdir().expect("lake");
    let cfg = DrainConfig::new(out.path(), "test");

    // A row AND an open field, so the document has both halves.
    let store = TestStore::slatedb_mem();
    {
        let c = collection(&store, log.path());
        c.put(
            &K1,
            &R {
                level: 7,
                name: "alpha".into(),
            },
        )
        .expect("the row");
        c.put(&K1, &Variant::Map(open_field("tier", "gold")))
            .expect("an open field");
    }
    drain(log.path(), &cfg).expect("window 1");

    // The whole document, replaced by a map of declared names only: the open half is deleted and
    // the row is written, in ONE commit.
    {
        let c = collection(&store, log.path());
        c.put(&K1, (&Variant::Map(declared_only(9, "beta")),))
            .expect("a whole-document put");
        c.put(
            &K2,
            &R {
                level: 5,
                name: "gamma".into(),
            },
        )
        .expect("another row");
        assert!(c.purge(&K2).expect("a whole-row removal"));
    }
    // BOTH removal shapes are in the log now: the replacement's slot-1 one and purge's slot-0.
    let report = drain(log.path(), &cfg).expect("window 2");
    let ops = ops_of(&report.files[0]);
    assert!(
        ops.contains(&("del".to_string(), Some(1))),
        "the writer emits a slot-1 removal: {ops:?}"
    );
    assert!(
        ops.contains(&("del".to_string(), Some(0))),
        "and a purge emits a slot-0 one: {ops:?}"
    );

    let table_dir = table_of(&report.files[0]);
    let state = read::current_state_at(&table_dir).expect("the table reads");

    // One row: the replaced document. The purged key is gone.
    assert_eq!(state.num_rows(), 1, "the slot-1 removal kept its row");
    let levels = state
        .column_by_name("level")
        .expect("the declared column")
        .as_any()
        .downcast_ref::<arrow::array::UInt32Array>()
        .expect("a UInt32 column");
    assert_eq!(levels.value(0), 9, "the row states the newest write");
    let names = state
        .column_by_name("name")
        .expect("the declared column")
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("a Utf8 column");
    assert_eq!(names.value(0), "beta");

    // ...and no open fields: the half the removal named is exactly what went.
    let variant = state
        .column_by_name("variant")
        .expect("the open-fields column");
    assert!(variant.is_null(0), "the open half is gone: {variant:?}");

    // The fold agrees with the full replay, and it is the same row.
    let folded = read::condense_at(&table_dir, 0).expect("a fold");
    assert_eq!(folded.rows, 1);
    let after = read::current_state_at(&table_dir).expect("the folded table reads");
    assert_eq!(after.num_rows(), 1);
    assert_eq!(
        after
            .column_by_name("okm_op")
            .expect("a folded file states its op")
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("an op column")
            .value(0),
        "put",
        "the surviving row is a full row, not an open-only one"
    );
}

#[test]
fn a_primary_removal_still_takes_the_whole_key() {
    let log = tempfile::tempdir().expect("log");
    let out = tempfile::tempdir().expect("lake");
    let cfg = DrainConfig::new(out.path(), "test");

    let store = TestStore::slatedb_mem();
    {
        let c = collection(&store, log.path());
        c.put(
            &K1,
            &R {
                level: 1,
                name: "alpha".into(),
            },
        )
        .expect("the row");
        c.put(&K1, &Variant::Map(open_field("tier", "gold")))
            .expect("an open field");
    }
    drain(log.path(), &cfg).expect("window 1");

    // A purge removes the row AND the open half, as two removals in one window: the row's removal
    // is what decides the key, and it comes first.
    {
        let c = collection(&store, log.path());
        assert!(c.purge(&K1).expect("purge"), "the document was there");
    }
    let report = drain(log.path(), &cfg).expect("window 2");
    let ops = ops_of(&report.files[0]);
    assert!(
        ops.contains(&("del".to_string(), Some(0))) && ops.contains(&("del".to_string(), Some(1))),
        "a purge states both halves: {ops:?}"
    );

    let table_dir = table_of(&report.files[0]);
    let state = read::current_state_at(&table_dir).expect("the table reads");
    assert_eq!(state.num_rows(), 0, "a primary removal still drops the key");

    // A fold of a table with nothing live SUCCEEDS and answers zero rows: there are ops to fold
    // (the two removals), they resolve to nothing, and no file replaces the ones they came in.
    // That differs from a file that already states final rows and carries no ops — piece (a)'s
    // publish case — where there is nothing to fold at all and the fold is refused.
    let folded = read::condense_at(&table_dir, 0).expect("a fold over removals");
    assert_eq!(folded.rows, 0, "the removals folded to nothing");
    assert!(
        folded.keys.is_empty(),
        "no current-state file replaces them: {folded:?}"
    );
    let remaining = okm_lake::delta::files_in_order_at(&table_dir).expect("the log reads");
    assert!(
        remaining.is_empty(),
        "the table states no data file: {remaining:?}"
    );
}

/// A change file built by hand, with the shape of an export written before `okm_slot` existed:
/// the slot column is absent, so every removal can only mean the whole row. Nothing may change
/// for that table.
#[test]
fn a_table_without_a_slot_column_reads_a_removal_as_the_whole_row() {
    use arrow::array::RecordBatch;
    use arrow::datatypes::{DataType, Field, Schema};

    let dir = tempfile::tempdir().expect("lake");
    let lake = LocalFs::new(dir.path());
    let table = "old";
    let keys = vec!["id".to_string()];

    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("okm_op", DataType::Utf8, false),
        Field::new("okm_seq", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(Int64Array::from(vec![1i64, 1])),
            std::sync::Arc::new(StringArray::from(vec!["put", "del"])),
            std::sync::Arc::new(arrow::array::UInt32Array::from(vec![0u32, 1])),
        ],
    )
    .expect("a batch");
    let bytes = okm_lake::snapshot::parquet_io::batch_bytes(&batch).expect("bytes");
    lake.put(&format!("{table}/data/old.parquet"), &bytes)
        .expect("a file");
    okm_lake::delta::commit_files(
        &lake,
        table,
        &[okm_lake::delta::Addition::plain(
            "data/old.parquet".to_string(),
            bytes.len() as u64,
        )],
        &keys,
    )
    .expect("a commit");

    let state = read::current_state(&lake, table).expect("the table reads");
    assert_eq!(
        state.num_rows(),
        0,
        "no slot column: a removal can only mean the whole row"
    );
}
