//! upsert_with — commanded RMW (PLAN Phase 6). Covers the insert path
//! (old = None), the RMW path (old in hand), and the invariant that the
//! whole write path fires through put: index entries, reduce folds and
//! subscribe emission all update without special-casing.
//!
//! Rows + the generated module live in subscribe_common.rs (shared with
//! subscribe_test — the generated enum's `super::` references must
//! resolve in every binary that includes it).

use okm_core::{MockStore, Table};

#[path = "subscribe_common.rs"]
mod subscribe_common;
use subscribe_common::*;
use subscribe_common::okm_subscribe;
use __OkmIndex_Counter_by_bucket as Counter_ByBucket;

#[test]
fn upsert_with_full_paths() {
    let mut t: Table<MockStore, CounterKey, Counter> = Table::new(MockStore::default(), 31);

    // ---------- insert path: old = None ----------
    let written = t.upsert_with(&CounterKey { id: 1 }, |old| {
        assert!(old.is_none(), "missing key must arrive as None");
        Counter { bucket: 7, hits: 10 }
    });
    assert_eq!(written.hits, 10);
    assert_eq!(t.get(&CounterKey { id: 1 }).unwrap().hits, 10);

    // ---------- RMW path: old in hand ----------
    let written = t.upsert_with(&CounterKey { id: 1 }, |old| {
        let mut c = old.expect("existing key must arrive as Some");
        c.hits += 5;
        c
    });
    assert_eq!(written.hits, 15);
    assert_eq!(t.get(&CounterKey { id: 1 }).unwrap().hits, 15);

    // ---------- write path fired: index entries present ----------
    // by_bucket(7) scan must see the row (index entry written by the put
    // inside upsert_with, not bypassed).
    let scanned = t.scan::<Counter_ByBucket>(&7u32.to_be_bytes());
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].1.as_ref().unwrap().hits, 15);

    // ---------- write path fired: reduce fold ----------
    // The reduce group for bucket 7 must total 15 (folds went through
    // __okm_apply_reduces inside put).
    let probe = Counter { bucket: 7, hits: 0 };
    let acc = okm_core::reduce_get::<_, CounterTotals>(t.store(), 31, &CounterKey { id: 1 }, &probe)
        .expect("group exists");
    assert_eq!(acc, CountSum { count: 1, sum: 15 });

    // ---------- write path fired: subscribe emission ----------
    let seen: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
    let sink = seen.clone();
    okm_subscribe::CHANNEL_ROWEVENT.register(move |ev: okm_subscribe::RowEvent| {
        let okm_subscribe::RowEvent::Counter(ev) = ev else { return false };
        sink.lock().unwrap().push(ev.row.hits);
        true
    });
    t.upsert_with(&CounterKey { id: 1 }, |old| {
        let mut c = old.expect("rmw");
        c.hits += 1;
        c
    });
    assert_eq!(*seen.lock().unwrap(), vec![16]);
}
