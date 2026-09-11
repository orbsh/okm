//! Subscribe channel integration (ADR-0008): `#[kv_subscribe]` annotated
//! rows emit events on the write path, build.rs collects them into the
//! `RowEvent` enum + `crate::okm_subscribe::CHANNEL`, and a registered
//! sink receives put/delete events. Also covers the bare per-row-type
//! cell fallback (no variant annotated) and no-sink drop semantics.

use okm_core::{KeyEncode, MockStore, Op, RowEncode, Table};

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

/// Subscribed row — routed through the build.rs-collected enum.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(AccountKey)]
#[kv_subscribe(RowEvent::Account)]
pub struct Account {
    pub balance: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(22)]
pub struct AuditKey {
    pub id: u64,
}

/// Un-annotated-variant row — falls back to the bare per-type cell.
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

/// Subscribed but nobody ever registers its bare cell — the no-sink
/// case must be tested on a channel no other test can touch (global
/// statics are process-wide; parallel tests would race otherwise).
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(GhostKey)]
#[kv_subscribe]
pub struct Ghost {
    pub v: u64,
}

#[test]
fn subscribe_round_trip() {
    // Consuming side: register the transport at assembly time. Here the
    // sink is just a queue; in a real app it forwards into a tokio mpsc,
    // crossbeam queue, etc. — transport is not the core's business.
    let seen: std::sync::Arc<std::sync::Mutex<Vec<(Op, u64)>>> = Default::default();
    let sink = seen.clone();
    crate::okm_subscribe::CHANNEL.register(move |ev: crate::okm_subscribe::RowEvent| {
        // Single-variant enum for now — the let-else documents the
        // fan-in shape more variants will take.
        #[allow(irrefutable_let_patterns)]
        let crate::okm_subscribe::RowEvent::Account(ev) = ev else {
            return false;
        };
        let mut q = sink.lock().unwrap();
        if q.len() >= 8 {
            return false; // simulate a bounded transport dropping
        }
        q.push((ev.op, ev.key.id));
        true
    });
    assert!(crate::okm_subscribe::CHANNEL.has_sink());

    let mut t: Table<MockStore, AccountKey, Account> = Table::new(MockStore::default(), 21);
    t.put(&AccountKey { id: 1 }, &Account { balance: 10 });
    t.put(&AccountKey { id: 2 }, &Account { balance: 20 });
    t.delete_by_pkey(&AccountKey { id: 1 });

    assert_eq!(
        *seen.lock().unwrap(),
        vec![(Op::Put, 1), (Op::Put, 2), (Op::Delete, 1)]
    );

    // Bare per-row-type cell fallback: same sink contract, own channel.
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    __OKM_CHANNEL_AUDIT.register(move |ev: okm_core::Event<AuditKey, Audit>| {
        assert!(matches!(ev.op, Op::Put | Op::Delete));
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    });
    let mut a: Table<MockStore, AuditKey, Audit> = Table::new(MockStore::default(), 22);
    a.put(&AuditKey { id: 7 }, &Audit { note: "hi".into() });
    a.delete_by_pkey(&AuditKey { id: 7 });
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn no_sink_drops_silently() {
    // A subscribed row with nobody consuming: writes must not block or
    // panic — the zero-cost default. A write round-trip works regardless
    // of event delivery.
    let mut t: Table<MockStore, GhostKey, Ghost> = Table::new(MockStore::default(), 23);
    t.put(&GhostKey { id: 3 }, &Ghost { v: 30 });
    assert!(t.get(&GhostKey { id: 3 }).is_some());
}
