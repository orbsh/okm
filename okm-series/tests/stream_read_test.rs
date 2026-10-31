#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Reading one stream, which is the primitive the union was built on.
//!
//! An offset is a stream's own counter (ADR-0046 §9), so "offset 0" means a different record
//! in every partition — `read(0, n)` applies it to all of them and answers the union in time
//! order. `read_stream` answers ONE of them, which is what a caller following a single tenant
//! or a single shard actually wants, and what it previously had to assemble by hand from
//! `parts()`.

use okm_series::{Body, Config, Log, Precision, Value};

const TENANT: u16 = 9;

fn partition_config() -> Config {
    Config {
        precision: Precision::Ms,
        partition_tag: Some(TENANT),
        ..Config::default()
    }
}

fn record(tenant: &str, index: u64) -> Body {
    let mut body = Body::new();
    body.push(TENANT, Value::Str(tenant.to_string()));
    body.push(1, Value::U64(index));
    body
}

fn tenant_of(body: &Body) -> &str {
    match body.get(TENANT) {
        Some(Value::Str(text)) => text,
        other => panic!("no tenant in the body: {other:?}"),
    }
}

fn populated() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let mut log = Log::open(&root, partition_config()).expect("open");
    // Interleaved, so a read that ignored the partition would return them mixed.
    for index in 0..3u64 {
        log.append(&record("eu", index)).expect("append eu");
        log.append(&record("us", index)).expect("append us");
    }
    log.flush().expect("flush");
    (dir, root)
}

#[test]
fn one_stream_reads_only_its_own_records_and_its_own_offsets() {
    let (_dir, root) = populated();
    let log = Log::open(&root, partition_config()).expect("reopen");

    for tenant in ["eu", "us"] {
        let rows = log.read_stream(Some(tenant), 0, 100).expect("read stream");
        assert_eq!(rows.len(), 3, "{tenant} has three records");
        for (_, _, body) in &rows {
            assert_eq!(tenant_of(body), tenant, "no other stream leaks in");
        }
        // The offsets are the stream's own counter, so both start at 0 and count the same
        // way — that is the whole reason reading one of them is worth having.
        let offsets: Vec<u64> = rows.iter().map(|(_, offset, _)| *offset).collect();
        assert_eq!(offsets, vec![0, 1, 2], "{tenant}'s offsets are its own");
    }

    // The union is still the union: six rows, in time order.
    let all = log.read(0, 100).expect("read all");
    assert_eq!(all.len(), 6);
    let tenants: Vec<&str> = all.iter().map(|(_, _, body)| tenant_of(body)).collect();
    assert!(tenants.contains(&"eu") && tenants.contains(&"us"));
}

#[test]
fn an_unknown_stream_answers_nothing() {
    let (_dir, root) = populated();
    let log = Log::open(&root, partition_config()).expect("reopen");
    assert!(
        log.read_stream(Some("nowhere"), 0, 100)
            .expect("read stream")
            .is_empty()
    );
}

#[test]
fn an_unpartitioned_log_has_exactly_one_stream() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let config = Config {
        precision: Precision::Ms,
        ..Config::default()
    };
    let mut log = Log::open(&root, config.clone()).expect("open");
    for index in 0..5u64 {
        log.append(&record("only", index)).expect("append");
    }
    log.flush().expect("flush");

    // `None` names that one stream; it is the same answer `read` gives.
    let stream = log.read_stream(None, 0, 100).expect("read stream");
    let all = log.read(0, 100).expect("read all");
    assert_eq!(stream.len(), 5);
    assert_eq!(stream, all);
    // A named value matches nothing here, because this log has no partition dirs at all.
    assert!(
        log.read_stream(Some("only"), 0, 100)
            .expect("read stream")
            .is_empty()
    );
}

#[test]
fn a_time_window_can_be_asked_of_one_stream() {
    let (_dir, root) = populated();
    let log = Log::open(&root, partition_config()).expect("reopen");

    let all = log.read(0, 100).expect("read all");
    let first = all.first().map(|(ts, _, _)| *ts).expect("rows");
    let last = all.last().map(|(ts, _, _)| *ts).expect("rows");
    let window = log
        .read_stream_by_time(Some("eu"), first, last, 100)
        .expect("read stream window");
    assert_eq!(window.len(), 3, "all three eu records fall in the window");
    for (_, _, body) in &window {
        assert_eq!(tenant_of(body), "eu");
    }

    // And a window that only covers part of the stream stays inside it.
    let narrow = log
        .read_stream_by_time(Some("us"), last + 1, last + 1_000, 100)
        .expect("read stream window");
    assert!(narrow.is_empty(), "nothing that late was written");
}
