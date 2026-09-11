//! Subscribe channel integration (ADR-0008): `#[kv_subscribe]` (bare)
//! annotated rows emit events on the write path, build.rs collects them
//! into the `RowEvent` enum (variant = row type name) +
//! `crate::okm_subscribe::CHANNEL`, and a registered sink receives
//! put/delete events. Also covers no-sink drop semantics.

use okm_core::{KeyEncode, MockStore, RowEncode, Table};

// build.rs-collected event enum + channel cell, generated into OUT_DIR.
// The derive expands to `crate::okm_subscribe::...`, so the module must
// sit at this file's root (integration tests: file = crate root).
mod okm_subscribe {
    include!(concat!(env!("OUT_DIR"), "/okm_subscribe.rs"));
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(21)]
pub struct AccountKey {
    pub id: u64,
}

/// Subscribed row — variant is the row type name, derived by build.rs.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(AccountKey)]
#[kv_subscribe]
pub struct Account {
    pub balance: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(22)]
pub struct AuditKey {
    pub id: u64,
}

/// Subscribed row routed through the same enum — fan-in shape.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(AuditKey)]
#[kv_subscribe]
pub struct Audit {
    pub note: String,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(23)]
pub struct GhostKey {
    pub id: u64,
}

/// Subscribed but nobody ever registers the channel — the no-sink
/// case must be tested on a channel no other test can touch (global
/// statics are process-wide; parallel tests would race otherwise).
/// This row carries its own enum alias via `#[kv_event_enum]`, which
/// lands as a second generated enum.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(GhostKey)]
#[kv_event_enum(ShadowEvents)]
#[kv_subscribe]
pub struct Ghost {
    pub v: u64,
}

#[test]
fn subscribe_round_trip() {
    // Consuming side: register the transport at assembly time. Here the
    // sink is just a queue; in a real app it forwards into a tokio mpsc,
    // crossbeam queue, etc. — transport is not the core's business.
    let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let sink = seen.clone();
    crate::okm_subscribe::CHANNEL_ROWEVENT.register(move |ev: crate::okm_subscribe::RowEvent| {
        let mut q = sink.lock().unwrap();
        if q.len() >= 8 {
            return false; // simulate a bounded transport dropping
        }
        // Multi-variant match — the exhaustiveness contract: every
        // subscribed row type must name its variant handling here.
        let (op, epoch, tag) = match ev {
            crate::okm_subscribe::RowEvent::Account(ev) => (ev.op, ev.epoch, ev.key.id),
            crate::okm_subscribe::RowEvent::Audit(ev) => (ev.op, ev.epoch, 100 + ev.key.id),
        };
        q.push(format!("{:?}#{epoch}#{tag}", op));
        true
    });
    assert!(crate::okm_subscribe::CHANNEL_ROWEVENT.has_sink());

    let mut t: Table<MockStore, AccountKey, Account> = Table::new(MockStore::default(), 21);
    t.put(&AccountKey { id: 1 }, &Account { balance: 10 });
    t.put(&AccountKey { id: 2 }, &Account { balance: 20 });
    t.delete_by_pkey(&AccountKey { id: 1 });

    // Epoch: the table's monotonic write-batch counter — 1, 2, 3 across
    // the three writes, giving consumers an exact same-table boundary.
    // Fan-in: the second row type lands in the same enum (tag 100+).
    let mut a: Table<MockStore, AuditKey, Audit> = Table::new(MockStore::default(), 22);
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
    let mut t: Table<MockStore, GhostKey, Ghost> = Table::new(MockStore::default(), 23);
    t.put(&GhostKey { id: 3 }, &Ghost { v: 30 });
    assert!(t.get(&GhostKey { id: 3 }).is_some());
}
