#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Where the monotone-time invariant lives, and why the log cannot break it.
//!
//! ADR-0046 §2: the log stamps the time itself and clamps to `max(clock, last)`, so a caller
//! holding a `Log` cannot feed it a record older than the last one — an NTP step becomes two
//! records with one timestamp, not a failed write. `Error::TimeBackwards` therefore never
//! comes out of the log face. It is not dead, though: the part-level writer owns the
//! invariant, and this test is the proof of both halves — the writer refuses, the log clamps.

use okm_series::format::{Codec, PartHeader, Precision};
use okm_series::part::{self, PartWriter};
use okm_series::{Body, Config, Error, Log, Value};

fn record(index: u64) -> Body {
    let mut body = Body::new();
    body.push(1, Value::U64(index));
    body
}

fn header() -> PartHeader {
    PartHeader {
        precision: Precision::Ms,
        bucket: 0,
        base_ts: 1_700_000_000_000,
        first_offset: 0,
        created_unix_ms: 1_700_000_000_000,
    }
}

#[test]
fn the_part_writer_refuses_a_record_older_than_its_predecessor() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = part::open_path(dir.path(), 0);
    let mut writer =
        PartWriter::create(&path, header(), 4096, 1, Codec::Raw, None).expect("create");

    writer
        .append(1_700_000_000_005, &record(0))
        .expect("the first record sets the clock");

    // One millisecond earlier. The byte order IS the time order, so this cannot be filed
    // somewhere plausible — the writer says so, and names both times.
    match writer.append(1_700_000_000_004, &record(1)) {
        Err(Error::TimeBackwards { last, given }) => {
            assert_eq!(
                last, 1_700_000_000_005,
                "the writer names its own last time"
            );
            assert_eq!(given, 1_700_000_000_004, "and the time it was handed");
        }
        other => panic!("a backwards record must be refused at the part level, got {other:?}"),
    }

    // The same time twice is fine: two records may share a timestamp, offsets order them.
    writer
        .append(1_700_000_000_005, &record(2))
        .expect("a repeated timestamp is not backwards");
    writer.seal().expect("seal");
}

#[test]
fn the_log_never_raises_it_because_it_clamps_instead() {
    // A clock that steps BACKWARDS mid-run — the NTP case — with the log's own clock hook.
    let now = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1_700_000_000_000));
    let handle = now.clone();
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let mut log = Log::open(
        &root,
        Config {
            precision: Precision::Ms,
            clock: Some(std::sync::Arc::new(move || {
                handle.load(std::sync::atomic::Ordering::SeqCst)
            })),
            ..Config::default()
        },
    )
    .expect("open");

    log.append(&record(0)).expect("append");
    // 5 seconds back. The write must succeed, not fail: a stepped clock is not the caller's
    // error (ADR-0046 §2), so the record carries the last time instead.
    now.fetch_sub(5_000, std::sync::atomic::Ordering::SeqCst);
    let offset = log
        .append(&record(1))
        .expect("a stepped clock is not a failed write");
    assert_eq!(offset, 1, "the record still landed, in order");
    log.flush().expect("flush what the writer buffered");

    let rows = log.read(0, 10).expect("read");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, rows[1].0, "both carry the clamped timestamp");
}
