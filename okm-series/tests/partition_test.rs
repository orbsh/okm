#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Partitioning by one field: a directory between the bucket and the parts, one writer
//! per partition, and offsets that still address one record in the whole log.

use okm_series::{Body, Codec, Config, DictConfig, Log, Precision, Value};

const PARTITION_TAG: u16 = 9;

fn config() -> Config {
    Config {
        precision: Precision::Ms,
        bucket_seconds: 3600,
        block_bytes: 4096,
        part_bytes: 64 * 1024 * 1024,
        zstd_level: 1,
        block_codec: Codec::Zstd,
        dict: Some(DictConfig::default()),
        partition_tag: Some(PARTITION_TAG),
        clock: Some(clock_step(BASE, 1)),
    }
}

fn body(partition: &str, message: &str) -> Body {
    let mut body = Body::new();
    body.push(1, Value::Str(message.to_string()));
    body.push(PARTITION_TAG, Value::Str(partition.to_string()));
    body
}

const BASE: u64 = 1_700_000_000_000;

/// A deterministic clock: `step` in the log's precision per call, starting at `start`.
fn clock_step(start: u64, step: u64) -> okm_series::Clock {
    let next = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(start));
    std::sync::Arc::new(move || next.fetch_add(step, std::sync::atomic::Ordering::Relaxed))
}

/// A clock that hands out these values in order, then repeats the last one.
fn clock_seq(values: &[u64]) -> okm_series::Clock {
    let queue = std::sync::Arc::new(std::sync::Mutex::new(values.to_vec()));
    std::sync::Arc::new(move || {
        let mut guard = match queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.len() > 1 {
            guard.remove(0)
        } else {
            guard.first().copied().unwrap_or(0)
        }
    })
}

#[test]
fn each_partition_gets_its_own_directory_and_part() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = BASE;
    for step in 0..6u64 {
        let partition = if step % 2 == 0 { "eu" } else { "us" };
        log.append(&body(partition, &format!("row {step}")))
            .unwrap();
    }
    log.seal().unwrap();

    let parts = log.parts().unwrap();
    assert_eq!(parts.len(), 2, "one part per partition value");
    let mut values: Vec<&str> = parts
        .iter()
        .map(|part| part.partition.as_deref().unwrap_or(""))
        .collect();
    values.sort_unstable();
    assert_eq!(values, vec!["eu", "us"]);

    // The layout on disk is <bucket>/p-<hex>/part-<n>.log, so a value can be any bytes
    // a body can hold.
    let bucket = dir.path().join(format!("{:012}", log.bucket_of(base)));
    assert!(bucket.join("p-6575").is_dir(), "eu is 6575 in hex");
    assert!(bucket.join("p-7573").is_dir(), "us is 7573 in hex");
}

#[test]
fn reads_walk_partitions_in_offset_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = BASE;
    for step in 0..10u64 {
        let partition = ["eu", "us", "ap"][(step % 3) as usize];
        log.append(&body(partition, &format!("row {step}")))
            .unwrap();
    }
    log.flush().unwrap();

    let rows = log.read(0, 100).unwrap();
    assert_eq!(rows.len(), 10);
    // Offsets are per stream (see ADR-0046 §9): each partition counts its own records,
    // because a block header carries only its first offset and the reader counts up.
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.0, base + index as u64, "rows come back in time order");
        let partition = ["eu", "us", "ap"][index % 3];
        assert_eq!(
            row.2.get(PARTITION_TAG).unwrap().as_str().unwrap(),
            partition
        );
        assert_eq!(row.1, (index / 3) as u64, "one counter per partition");
    }
    assert_eq!(rows[7].2.get(1).unwrap().as_str().unwrap(), "row 7");
}

#[test]
fn a_time_range_read_crosses_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let base = BASE;
    let mut config = config();
    config.clock = Some(clock_step(base, 1_000));
    let mut log = Log::open(dir.path(), config).unwrap();
    for step in 0..12u64 {
        let partition = if step < 6 { "eu" } else { "us" };
        log.append(&body(partition, &format!("row {step}")),
        )
        .unwrap();
    }
    log.flush().unwrap();

    let rows = log.read_by_time(base + 5_000, base + 7_000, 100).unwrap();
    assert_eq!(
        rows.len(),
        3,
        "inclusive on both ends, across both partitions"
    );
    assert_eq!(rows[0].0, base + 5_000);
    assert_eq!(rows[0].1, 5, "the fifth record of the eu stream");
    assert_eq!(rows[2].0, base + 7_000);
    assert_eq!(
        rows[2].1, 1,
        "the second record of the us stream: offsets are per stream"
    );
}

#[test]
fn a_record_without_the_partition_field_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let mut body = Body::new();
    body.push(1, Value::Str("no partition here".to_string()));
    let refused = log.append(&body);
    assert!(
        matches!(refused, Err(okm_series::Error::MissingPartition(tag)) if tag == PARTITION_TAG),
        "a partitioned log will not file a record somewhere plausible"
    );
}

#[test]
fn retention_takes_every_partition_of_a_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config();
    // 1970-01-01T00:00 and 02:00 written to both partitions, so each has two buckets.
    config.clock = Some(clock_seq(&[1_000, 1_000, 7_200_001, 7_200_001]));
    let mut log = Log::open(dir.path(), config).unwrap();
    for partition in ["eu", "us"] {
        log.append(&body(partition, "first hour")).unwrap();
    }
    for partition in ["eu", "us"] {
        log.append(&body(partition, "third hour")).unwrap();
    }
    log.seal().unwrap();
    assert_eq!(log.parts().unwrap().len(), 4, "two buckets, two partitions");

    let dropped = log.retain_before(7_200_000).unwrap();
    assert_eq!(dropped.len(), 1);
    let parts = log.parts().unwrap();
    assert_eq!(parts.len(), 2, "the later bucket keeps both partitions");
    assert!(
        parts.iter().all(|part| part.bucket == 2),
        "retention is a bucket decision, not a partition one"
    );
}

#[test]
fn the_partition_field_is_declared_once() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut log = Log::open(dir.path(), config()).unwrap();
        log.append(&body("eu", "first")).unwrap();
        log.flush().unwrap();
    }
    // Same declaration: fine, and the open part is adopted.
    let log = Log::open(dir.path(), config()).unwrap();
    assert_eq!(log.meta().partition_tag, Some(PARTITION_TAG));
    assert_eq!(log.read(0, 10).unwrap().len(), 1);

    // Different declaration: refused, because a reader that cannot tell which layout a
    // directory is in cannot read it.
    let mut other = config();
    other.partition_tag = Some(3);
    let mismatch = Log::open(dir.path(), other);
    assert!(matches!(
        mismatch,
        Err(okm_series::Error::ConfigMismatch(_))
    ));
}
