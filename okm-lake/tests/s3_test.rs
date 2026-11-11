//! The bucket face of [`okm_lake::store`], exercised only when one is configured.
//!
//! A lake is a key space, and the interesting claims about the S3 side are the ones a local
//! directory cannot check: an object is claimed once (`Create`), a list is a prefix scan, and
//! a commit survives a fresh handle. Set:
//!
//! ```text
//! OKM_S3_URL=s3://bucket/prefix
//! OKM_S3_ENDPOINT=http://127.0.0.1:9000
//! OKM_S3_ACCESS_KEY=...
//! OKM_S3_SECRET=...
//! ```
//!
//! With the variables unset the test reports that it skipped rather than failing: a checkout
//! with no bucket is a normal checkout.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_lake::store::{self, S3Options};

fn configured() -> Option<(String, S3Options)> {
    let url = std::env::var("OKM_S3_URL").ok()?;
    let access_key = std::env::var("OKM_S3_ACCESS_KEY").ok()?;
    let secret_key = std::env::var("OKM_S3_SECRET").ok()?;
    Some((
        url,
        S3Options {
            endpoint: std::env::var("OKM_S3_ENDPOINT").ok(),
            region: std::env::var("OKM_S3_REGION").ok(),
            access_key,
            secret_key,
        },
    ))
}

/// One `Int64` column, so the Delta schema is computable from the file itself.
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

#[test]
fn a_bucket_takes_a_publish_and_gives_it_back() {
    let Some((url, options)) = configured() else {
        println!("skipped: OKM_S3_URL is not set");
        return;
    };
    let lake = store::open(&url, Some(options)).expect("open the bucket");
    // One prefix per run: the test cleans up after itself and never touches anybody's keys.
    let table = format!("selftest-{}", std::process::id());
    let keys = vec!["id".to_string()];

    let first =
        okm_lake::publish::replace_table(&*lake, &table, "part-1.parquet", &parquet(3), &keys)
            .expect("first publish");
    assert_eq!(first.commit.version, 0);

    // A second publish replaces the file, and the object it replaced is gone.
    let second =
        okm_lake::publish::replace_table(&*lake, &table, "part-2.parquet", &parquet(5), &keys)
            .expect("second publish");
    assert_eq!(second.commit.version, 1);
    assert_eq!(second.replaced, 1);
    assert!(
        lake.head(&format!("{table}/data/part-1.parquet"))
            .unwrap()
            .is_none()
    );
    assert!(
        lake.head(&format!("{table}/data/part-2.parquet"))
            .unwrap()
            .is_some()
    );

    // The log is what a reader reads: two versions, and the newest names one live file.
    let log = lake.list(&format!("{table}/_delta_log/")).unwrap();
    assert_eq!(log.len(), 2, "two commits: {log:?}");
    let live = okm_lake::delta::files_in_order(&*lake, &table).expect("read the log back");
    assert_eq!(live, vec!["data/part-2.parquet".to_string()]);

    // The bytes that came back are a Parquet file, not a truncated object.
    let bytes = lake.get(&format!("{table}/data/part-2.parquet")).unwrap();
    assert_eq!(&bytes[..4], b"PAR1");

    // A create-only claim on a version that exists is refused, not overwritten.
    assert!(
        !lake
            .put_new(
                &format!("{table}/_delta_log/00000000000000000000.json"),
                b"x"
            )
            .unwrap()
    );

    for entry in lake.list(&format!("{table}/")).unwrap() {
        lake.delete(&entry.path).unwrap();
    }
    assert!(lake.list(&format!("{table}/")).unwrap().is_empty());
}
