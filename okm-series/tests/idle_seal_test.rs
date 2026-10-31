#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! An idle log's part, and the two things that end it.
//!
//! A part is sealed by a bucket crossing, by `part_bytes`, or by `seal()`. None of those
//! happens to a log that stops writing, and ADR-0046 §3 rules out a timer — so the answers
//! are lazy, driven by a call the caller already makes: `retain_before` (which cannot drop a
//! bucket it is still holding open) and the next `open` (which finds a part in a bucket the
//! clock has left). Both are tested here through the `clock` hook, so "an hour later" is a
//! number rather than a sleep.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use okm_series::{Body, Config, Log, Precision, Value};

/// A clock the test moves by hand, in the log's precision (milliseconds here).
fn stepped(start: u64) -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(start))
}

fn config(now: &Arc<AtomicU64>) -> Config {
    let handle = now.clone();
    Config {
        precision: Precision::Ms,
        // One second per bucket, so a bucket is a number the test can reason about.
        bucket_seconds: 1,
        clock: Some(Arc::new(move || handle.load(Ordering::SeqCst))),
        ..Config::default()
    }
}

fn record(index: u64) -> Body {
    let mut body = Body::new();
    body.push(1, Value::U64(index));
    body
}

#[test]
fn retention_seals_the_part_it_is_about_to_drop() {
    let now = stepped(1_700_000_000_000);
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");

    let mut log = Log::open(&root, config(&now)).expect("open");
    for index in 0..3 {
        log.append(&record(index)).expect("append");
    }
    log.flush().expect("flush");
    let before = log.parts().expect("parts");
    assert_eq!(before.len(), 1);
    assert!(!before[0].sealed, "nothing seals an idle log");

    // The caller's retention call IS the tick: the bucket it asks to drop is the bucket the
    // live writer holds, so the writer has to end before the drop can happen.
    now.fetch_add(3_000, Ordering::SeqCst);
    let dropped = log
        .retain_before(1_700_000_000_000 + 3_000)
        .expect("retain");
    assert_eq!(dropped.len(), 1, "the idle bucket was dropped: {dropped:?}");
    assert!(
        log.parts().expect("parts").is_empty(),
        "the directory went away"
    );
}

#[test]
fn reopening_seals_a_part_in_a_bucket_the_clock_has_left() {
    let now = stepped(1_700_000_000_000);
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");

    let mut log = Log::open(&root, config(&now)).expect("open");
    for index in 0..3 {
        log.append(&record(index)).expect("append");
    }
    log.flush().expect("flush");
    drop(log);

    // An hour later, the same directory: the part's bucket is behind the clock, so the next
    // open finishes it — with a footer, which is what a reader and retention both need.
    now.fetch_add(3_600_000, Ordering::SeqCst);
    let mut log = Log::open(&root, config(&now)).expect("reopen");
    let parts = log.parts().expect("parts");
    assert_eq!(parts.len(), 1);
    assert!(parts[0].sealed, "the old part was sealed at open");
    let rows = log.read(0, 100).expect("read");
    assert_eq!(rows.len(), 3, "sealing kept every record");

    // And retention can now drop it, because no writer holds it.
    let dropped = log
        .retain_before(1_700_000_000_000 + 3_600_000)
        .expect("retain");
    assert_eq!(dropped.len(), 1);
}

#[test]
fn reopening_in_the_same_bucket_keeps_the_part_open() {
    let now = stepped(1_700_000_000_000);
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");

    let mut log = Log::open(&root, config(&now)).expect("open");
    log.append(&record(0)).expect("append");
    log.flush().expect("flush");
    drop(log);

    // 400 ms later is the same second: the part is still where records go, so it must NOT be
    // sealed — sealing is a bucket decision, not "some time passed".
    now.fetch_add(400, Ordering::SeqCst);
    let mut log = Log::open(&root, config(&now)).expect("reopen");
    let parts = log.parts().expect("parts");
    assert_eq!(parts.len(), 1);
    assert!(!parts[0].sealed, "same bucket, same part");

    log.append(&record(1))
        .expect("append into the adopted part");
    log.flush().expect("flush");
    let parts = log.parts().expect("parts");
    assert_eq!(parts.len(), 1, "still one part: the append reused it");
    assert_eq!(parts[0].records, 2);
}
