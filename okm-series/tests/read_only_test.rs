#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Reading a live log: a reader takes no lock, so a writer and a reader can hold the same
//! directory at once.
//!
//! The format allows it for a reason worth restating here — a writer writes a block's bytes and
//! only then appends the block's row to the index, and the index parser drops a half-written
//! row at the end. So a reader sees whole blocks, never a block that is still being written,
//! and its answer is at most one block behind the writer. These tests hold that: the reader
//! opens while the writer holds the log, sees what is flushed, does not see what is still
//! buffered (and does not error on it), and sees it after the flush.

use okm_series::{Body, Config, Error, Log, LogReader, Precision, Value};

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
fn a_reader_opens_while_a_writer_holds_the_log() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let mut log = Log::open(&root, config()).expect("open for writing");

    for index in 0..3 {
        log.append(&record(index)).expect("append");
    }
    log.flush().expect("flush");

    // The writer is alive and holds the LOCK and the in-process claim. A reader must still be
    // able to open: it takes neither, and it writes nothing.
    let reader = LogReader::open(&root, config()).expect("open for reading");
    let rows = reader.read(0, 100).expect("read");
    assert_eq!(rows.len(), 3);
    assert_eq!(reader.meta().bucket_seconds, config().bucket_seconds);

    // And it is not a stale snapshot by construction: reading again after more writes sees
    // them, because the cached reader is keyed by the file's length.
    log.append(&record(3)).expect("append");
    log.flush().expect("flush");
    assert_eq!(reader.read(0, 100).expect("read again").len(), 4);
}

#[test]
fn a_reader_does_not_see_a_record_that_is_still_buffered() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let mut log = Log::open(&root, config()).expect("open for writing");
    log.append(&record(0)).expect("append");
    log.flush().expect("flush");

    let reader = LogReader::open(&root, config()).expect("open for reading");
    assert_eq!(reader.read(0, 100).expect("read").len(), 1);

    // Appended but not flushed: the block does not exist yet, so there is no index row for it.
    // The reader must answer one record — not error, and not two.
    log.append(&record(1)).expect("append");
    let rows = reader.read(0, 100).expect("read while buffered");
    assert_eq!(rows.len(), 1, "an unflushed record is not on disk yet");

    log.flush().expect("flush");
    assert_eq!(reader.read(0, 100).expect("read after flush").len(), 2);
}

#[test]
fn a_reader_reads_each_stream_on_its_own() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let partitioned = Config {
        precision: Precision::Ms,
        partition_tag: Some(9),
        ..Config::default()
    };
    let mut log = Log::open(&root, partitioned.clone()).expect("open for writing");
    for (tenant, index) in [("eu", 0u64), ("us", 0), ("eu", 1)] {
        let mut body = Body::new();
        body.push(9, Value::Str(tenant.to_string()));
        body.push(1, Value::U64(index));
        log.append(&body).expect("append");
    }
    log.flush().expect("flush");

    let reader = LogReader::open(&root, partitioned).expect("open for reading");
    let eu = reader.read_stream(Some("eu"), 0, 100).expect("read stream");
    assert_eq!(eu.len(), 2);
    assert_eq!(
        eu.iter().map(|(_, offset, _)| *offset).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(reader.read(0, 100).expect("read all").len(), 3);
    assert_eq!(reader.parts().expect("parts").len(), 2, "two partitions");
}

#[test]
fn a_directory_that_is_not_a_log_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    // No `log.meta`: a reader cannot create what it is only allowed to read.
    match LogReader::open(dir.path(), config()) {
        Err(Error::BadMeta(what)) => assert!(what.contains("log.meta"), "{what}"),
        other => panic!("an empty directory is not a log, got {other:?}"),
    }
}

#[test]
fn a_reader_checks_the_identity_it_is_asked_for() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let _log = Log::open(&root, config()).expect("open for writing");

    let wrong = Config {
        bucket_seconds: 60,
        ..config()
    };
    match LogReader::open(&root, wrong) {
        Err(Error::ConfigMismatch(what)) => assert!(what.contains("bucket"), "{what}"),
        other => panic!("a reader must refuse a different identity, got {other:?}"),
    }
}
