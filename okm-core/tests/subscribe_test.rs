//! Subscribe channel integration (ADR-0008): `#[kv_subscribe]` (bare)
//! annotated rows emit events on the write path, build.rs collects them
//! into the `RowEvent` enum (variant = row type name) +
//! `crate::okm_subscribe::CHANNEL_ROWEVENT`, and a registered sink
//! receives put/delete events. Also covers no-sink drop semantics.
//!
//! Row types + the generated module live in subscribe_common.rs (shared
//! with upsert_test — the generated enum's `super::` references must
//! resolve in every binary that includes it).

use okm_core::{MockStore, Table};

#[path = "subscribe_common.rs"]
mod subscribe_common;
use subscribe_common::*;
use subscribe_common::okm_subscribe;

#[test]
fn subscribe_round_trip() {
    // Consuming side: register the transport at assembly time. Here the
    // sink is just a queue; in a real app it forwards into a tokio mpsc,
    // crossbeam queue, etc. — transport is not the core's business.
    let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let sink = seen.clone();
    okm_subscribe::CHANNEL_ROWEVENT.register(move |ev: okm_subscribe::RowEvent| {
        let mut q = sink.lock().unwrap();
        if q.len() >= 8 {
            return false; // simulate a bounded transport dropping
        }
        // Multi-variant match — the exhaustiveness contract: every
        // subscribed row type must name its variant handling here.
        let (op, epoch, tag) = match ev {
            okm_subscribe::RowEvent::Account(ev) => (ev.op, ev.epoch, ev.key.id),
            okm_subscribe::RowEvent::Audit(ev) => (ev.op, ev.epoch, 100 + ev.key.id),
            okm_subscribe::RowEvent::Counter(ev) => (ev.op, ev.epoch, 200 + ev.key.id),
        };
        q.push(format!("{:?}#{epoch}#{tag}", op));
        true
    });
    assert!(okm_subscribe::CHANNEL_ROWEVENT.has_sink());

    let mut t: Table<MockStore, AccountKey, Account> = Table::new(MockStore::default());
    t.put(&AccountKey { id: 1 }, &Account { balance: 10 });
    t.put(&AccountKey { id: 2 }, &Account { balance: 20 });
    t.delete_by_pkey(&AccountKey { id: 1 });

    // Epoch: the table's monotonic write-batch counter — 1, 2, 3 across
    // the three writes, giving consumers an exact same-table boundary.
    // Fan-in: the second row type lands in the same enum (tag 100+).
    let mut a: Table<MockStore, AuditKey, Audit> = Table::new(MockStore::default());
    a.put(&AuditKey { id: 7 }, &Audit { note: "hi".into() });
    a.delete_by_pkey(&AuditKey { id: 7 });

    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "Put#1#1".to_string(),
            "Put#2#2".to_string(),
            "Delete#3#1".to_string(),
            "Put#1#107".to_string(),
            "Delete#2#107".to_string(),
        ]
    );
}

#[test]
fn no_sink_drops_silently() {
    // A subscribed row with nobody consuming: writes must not block or
    // panic — the zero-cost default. A write round-trip works regardless
    // of event delivery. (Ghost rides its own `ShadowEvents` enum, so
    // this test cannot race the RowEvent consumers above.)
    let mut t: Table<MockStore, GhostKey, Ghost> = Table::new(MockStore::default());
    t.put(&GhostKey { id: 3 }, &Ghost { v: 30 });
    assert!(t.get(&GhostKey { id: 3 }).is_some());
}
