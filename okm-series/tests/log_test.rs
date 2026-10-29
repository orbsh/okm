#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The envelope: records round-trip, a torn tail is cut, a seal is a rename, and
//! retention is a directory that stops existing.

use std::io::Write;

use okm_series::{Body, Config, Log, Precision, Value};

fn config() -> Config {
    Config {
        precision: Precision::Ms,
        bucket_seconds: 3600,
        block_bytes: 64 * 1024,
        part_bytes: 64 * 1024 * 1024,
        zstd_level: 1,
        block_codec: okm_series::Codec::Zstd,
        dict: Some(okm_series::DictConfig::default()),
        partition_tag: None,
    }
}

fn body(level: &str, message: &str) -> Body {
    let mut body = Body::new();
    body.push(1, Value::Str(level.to_string()));
    body.push(2, Value::Str(message.to_string()));
    body
}

#[test]
fn records_come_back_in_time_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = 1_700_000_000_000u64;
    for step in 0..3 {
        let offset = log
            .append(base + step * 250, &body("info", &format!("row {step}")))
            .unwrap();
        assert_eq!(offset, step);
    }
    log.seal().unwrap();

    let rows = log.read(0, 10).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].0, base);
    assert_eq!(rows[2].0, base + 500);
    assert_eq!(rows[1].2.get(2).unwrap().as_str().unwrap(), "row 1");
}

#[test]
fn time_cannot_go_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    log.append(2_000, &body("a", "first")).unwrap();
    let refused = log.append(1_999, &body("a", "second"));
    assert!(
        refused.is_err(),
        "an older record must be refused, not clamped"
    );
}

#[test]
fn blocks_rotate_and_the_index_still_finds_the_offset() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config();
    config.block_bytes = 96; // a few records per block
    let mut log = Log::open(dir.path(), config).unwrap();
    let base = 1_700_000_000_000u64;
    for step in 0..40u64 {
        log.append(
            base + step,
            &body("info", &format!("message number {step}")),
        )
        .unwrap();
    }
    log.seal().unwrap();

    let parts = log.parts().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].records, 40, "the footer counts every record");

    let rows = log.read(37, 10).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].1, 37);
    assert_eq!(
        rows[0].2.get(2).unwrap().as_str().unwrap(),
        "message number 37"
    );
}

#[test]
fn a_torn_tail_is_cut_and_the_log_keeps_appending() {
    let dir = tempfile::tempdir().unwrap();
    let base = 1_700_000_000_000u64;
    {
        let mut log = Log::open(dir.path(), config()).unwrap();
        for step in 0..5u64 {
            log.append(base + step, &body("info", &format!("row {step}")))
                .unwrap();
        }
        // Durability boundary, then a crash: half a block of garbage lands at the end.
        log.append_durable(base + 5, &body("info", "row 5"))
            .unwrap();
        let part = find_open_part(dir.path()).expect("an open part");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .unwrap();
        file.write_all(&[0xAB; 37]).unwrap();
        file.sync_all().unwrap();
    }

    let mut log = Log::open(dir.path(), config()).unwrap();
    let rows = log.read(0, 100).unwrap();
    assert_eq!(rows.len(), 6, "the garbage is not a record");
    // Offsets continue from the recovered tail, not from the last sealed part.
    let next = log
        .append_durable(base + 6, &body("info", "row 6"))
        .unwrap();
    assert_eq!(next, 6);
    let rows = log.read(0, 100).unwrap();
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[6].0, base + 6);
}

#[test]
fn sealing_is_a_rename_and_the_footer_carries_the_totals() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = 1_700_000_000_000u64;
    log.append(base, &body("info", "one")).unwrap();
    log.append(base + 1, &body("info", "two")).unwrap();
    let footers = log.seal().unwrap();
    assert_eq!(footers.len(), 1, "one open part, one footer");
    assert_eq!(footers[0].records, 2);
    assert_eq!(footers[0].last_ts, base + 1);
    assert_eq!(footers[0].blocks, 1);

    let parts = log.parts().unwrap();
    assert_eq!(parts.len(), 1);
    assert!(parts[0].sealed);
    assert!(parts[0].path.to_string_lossy().ends_with(".log"));
    assert!(find_open_part(dir.path()).is_none());
}

#[test]
fn a_flush_is_a_durability_boundary_without_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let base = 1_700_000_000_000u64;
    {
        let mut log = Log::open(dir.path(), config()).unwrap();
        for step in 0..3u64 {
            log.append(base + step, &body("info", "batched")).unwrap();
        }
        // Nothing is visible to a reader until this returns.
        log.flush().unwrap();
    }
    let log = Log::open(dir.path(), config()).unwrap();
    let rows = log.read(0, 10).unwrap();
    assert_eq!(rows.len(), 3, "a flushed batch survives a reopen");
}

#[test]
fn moving_into_a_later_bucket_finishes_the_previous_part() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    // 1970-01-01T00:00 then 02:00: the first bucket's part must be sealed, or retention
    // could never drop that bucket (it refuses to touch a bucket that still has a writer).
    log.append(1_000, &body("info", "first hour")).unwrap();
    log.append(7_200_000 + 1_000, &body("info", "third hour"))
        .unwrap();

    let parts = log.parts().unwrap();
    assert_eq!(parts.len(), 2);
    assert!(
        parts
            .iter()
            .filter(|part| part.bucket == 0)
            .all(|part| part.sealed),
        "crossing a bucket boundary seals what came before it"
    );
    assert_eq!(log.retain_before(7_200_000).unwrap(), vec![0]);
}

#[test]
fn retention_drops_a_bucket_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    // 1970-01-01T00:00 and 02:00, two buckets.
    log.append(1_000, &body("info", "first hour")).unwrap();
    log.append(7_200_000 + 1_000, &body("info", "third hour"))
        .unwrap();
    log.seal().unwrap();

    let before = log.parts().unwrap();
    assert_eq!(before.len(), 2, "two buckets means two parts");

    let dropped = log.retain_before(7_200_000).unwrap();
    assert_eq!(dropped.len(), 1);
    let after = log.parts().unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].bucket, 2);
    let rows = log.read(0, 10).unwrap();
    assert_eq!(rows.len(), 1, "what retention dropped stays dropped");
}

#[test]
fn a_time_range_reads_only_what_it_covers() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = 1_700_000_000_000u64;
    for step in 0..10u64 {
        log.append(base + step * 1_000, &body("info", &format!("row {step}")))
            .unwrap();
    }
    log.seal().unwrap();

    let rows = log.read_by_time(base + 3_000, base + 5_000, 100).unwrap();
    assert_eq!(rows.len(), 3, "inclusive on both ends");
    assert_eq!(rows[0].1, 3);
    assert_eq!(rows[2].1, 5);
}

#[test]
fn a_time_range_reads_an_unsealed_part_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = 1_700_000_000_000u64;
    // Everything in one block, nothing sealed: the part's time span has to come from its
    // last block, not from its last index row (which is the block's FIRST record).
    for step in 0..6u64 {
        log.append(base + step * 1_000, &body("info", &format!("row {step}")))
            .unwrap();
    }
    log.flush().unwrap();

    let rows = log.read_by_time(base + 4_000, base + 5_000, 100).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "the part must not be skipped as out of range"
    );
    assert_eq!(rows[0].1, 4);
    assert_eq!(rows[1].1, 5);
}

#[test]
fn repetitive_bodies_compress_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = 1_700_000_000_000u64;
    let message =
        "the same long line, over and over, because that is what a log looks like".repeat(4);
    for step in 0..40u64 {
        log.append(base + step, &body("error", &message)).unwrap();
    }
    log.seal().unwrap();
    let parts = log.parts().unwrap();
    let on_disk = parts[0].bytes;
    let raw = 40 * message.len() as u64;
    assert!(
        on_disk < raw / 2,
        "40 identical bodies are {raw} bytes raw, {on_disk} on disk"
    );
    let rows = log.read(0, 100).unwrap();
    assert_eq!(rows.len(), 40);
    assert_eq!(rows[0].2.get(2).unwrap().as_str().unwrap(), message);
}

fn find_open_part(root: &std::path::Path) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        for inner in std::fs::read_dir(entry.path()).unwrap() {
            let inner = inner.unwrap();
            let path = inner.path();
            if path.to_string_lossy().ends_with(".open") {
                return Some(path);
            }
        }
    }
    None
}
