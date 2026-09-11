//! Subscribe channel integration (ADR-0008): `#[kv_subscribe]` annotated
//! rows emit events on the write path, build.rs collects them into the
//! `RowEvent` enum + `::okm_subscribe::CHANNEL`, and a registered sink
//! receives put/delete events. Also covers the bare per-row-type cell
//! fallback (no variant annotated) and no-sink drop semantics.

use okm::{KeyEncode, MockStore, Op, RowEncode, Table};

// build.rs-collected event enum + channel cell, generated into OUT_DIR.
// The derive expands to `::okm_subscribe::...`, so the module must sit at
// this file's root (integration tests: file = crate root).
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

#[test]
fn enum_channel_receives_put_and_delete() {
    // Consuming side: register the transport at assembly time. Here the
    // sink is just a queue; in a real app it forwards into a tokio mpsc,
    // crossbeam queue, etc. — transport is not the core's business.
    let seen: std::sync::Arc<std::sync::Mutex<Vec<(Op, u64)>>> = Default::default();
    let sink = seen.clone();
    ::okm_subscribe::CHANNEL.register(move |ev: ::okm_subscribe::RowEvent| {
        let ::okm_subscribe::RowEvent::Account(ev) = ev else {
            return false;
        };
        let mut q = sink.lock().unwrap();
        if q.len() >= 8 {
            return false; // simulate a bounded transport dropping
        }
        q.push((ev.op, ev.key.id));
        true
    });
    assert!(::okm_subscribe::CHANNEL.has_sink());

    let mut t: Table<MockStore, AccountKey, Account> = Table::new(MockStore::default(), 21);
    t.put(&AccountKey { id: 1 }, &Account { balance: 10 });
    t.put(&AccountKey { id: 2 }, &Account { balance: 20 });
    t.delete_by_pkey(&AccountKey { id: 1 });

    assert_eq!(
        *seen.lock().unwrap(),
        vec![(Op::Put, 1), (Op::Put, 2), (Op::Delete, 1)]
    );
}

#[test]
fn bare_cell_receives_events_without_enum() {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    __OkmChannel_Audit.register(move |ev: okm::Event<AuditKey, Audit>| {
        assert!(matches!(ev.op, Op::Put | Op::Delete));
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    });

    let mut t: Table<MockStore, AuditKey, Audit> = Table::new(MockStore::default(), 22);
    t.put(&AuditKey { id: 7 }, &Audit { note: "hi".into() });
    t.delete_by_pkey(&AuditKey { id: 7 });
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn no_sink_drops_silently() {
    // A subscribed row with nobody consuming: writes must not block or
    // panic — the zero-cost default. A write round-trip works regardless
    // of event delivery.
    let mut t: Table<MockStore, AccountKey, Account> = Table::new(MockStore::default(), 21);
    t.put(&AccountKey { id: 3 }, &Account { balance: 30 });
    assert!(t.get(&AccountKey { id: 3 }).is_some());
}
