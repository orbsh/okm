#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The reader cache, and the one thing it must never do.
//!
//! A point read costs 62 µs, of which 11 is opening the part: header, footer, dictionary and
//! a 70 KB index. Paying that once per part instead of once per call is the whole cache. The
//! danger is a stale index: a part is append-only, so an open part GROWS, and a reader parsed
//! from the bytes it had a moment ago knows nothing about the blocks written since. The
//! validity key is the file's length, and these tests are why it has to be.

use okm_series::{Body, Config, Log, Precision, Value};

fn record(index: u64) -> Body {
    let mut body = Body::new();
    body.push(1, Value::U64(index));
    body
}

fn config() -> Config {
    Config {
        precision: Precision::Ms,
        ..Config::default()
    }
}

#[test]
fn a_read_sees_records_written_after_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let mut log = Log::open(&root, config()).expect("open");

    for index in 0..3 {
        log.append(&record(index)).expect("append");
    }
    log.flush().expect("flush");
    // The first read parses the part and caches it under the length it had at that moment.
    assert_eq!(log.read(0, 100).expect("read").len(), 3);

    for index in 3..5 {
        log.append(&record(index)).expect("append");
    }
    log.flush().expect("flush");

    // The part grew. A cache that ignored the length would answer three records here — the
    // exact shape of a stale index: silent, plausible, and missing the tail.
    let rows = log.read(0, 100).expect("read");
    assert_eq!(rows.len(), 5, "the appended records are visible");
    assert_eq!(
        rows.iter()
            .map(|(_, offset, _)| *offset)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
}

#[test]
fn parts_reflects_growth_too() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let mut log = Log::open(&root, config()).expect("open");

    // Small blocks, so a flush actually produces a block per few records.
    log.append(&record(0)).expect("append");
    log.flush().expect("flush");
    let before = log.parts().expect("parts");
    assert_eq!(before.len(), 1);
    let first_records = before[0].records;
    let first_bytes = before[0].bytes;

    log.append(&record(1)).expect("append");
    log.flush().expect("flush");
    let after = log.parts().expect("parts");
    assert_eq!(after.len(), 1, "still one part");
    assert_eq!(
        after[0].records,
        first_records + 1,
        "the reader was re-parsed"
    );
    assert!(after[0].bytes > first_bytes, "and it grew on disk");
}

#[test]
fn a_sealed_part_reads_the_same_every_time() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let mut log = Log::open(&root, config()).expect("open");
    for index in 0..4 {
        log.append(&record(index)).expect("append");
    }
    log.seal().expect("seal");

    // Sealing renames the file, so the second read arrives at a different key — the cache has
    // to answer from the new name, not from the entry the open part left behind.
    let first = log.read(0, 100).expect("read");
    let again = log.read(0, 100).expect("read again");
    assert_eq!(first, again);
    assert_eq!(first.len(), 4);

    let parts = log.parts().expect("parts");
    assert!(
        parts[0].sealed,
        "the part under the new name is the sealed one"
    );
    assert_eq!(parts[0].records, 4);
}
