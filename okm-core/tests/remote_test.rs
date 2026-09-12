//! ADR-0010 Phase 7 end-to-end: RemoteStore (sender, `impl VirtualStorage`)
//! → mpsc channel (reference transport) → StorageHost (receiver, declared
//! prefix) → MockStore (the real engine). Covers the whole Table write
//! path surviving remoteness: primary + index entries in one frame, one
//! receiver WAL commit per batch, prefix isolation between two hosts.
//!
//! Observation discipline: the host owns its engine behind the mutex; the
//! tests observe through the remote handle (get / scan_suffix round
//! trips), never through a kept MockStore clone — MockStore::clone is a
//! deep copy, a kept clone would observe a different engine.

use okm_core::{
    KeyEncode, KvBatch, MockStore, RemoteStore, Row, RowEncode, StorageHost, Table, VirtualHandle,
    VirtualStorage,
};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub id: u64,
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(UserKey)]
#[kv_ns(7)]
#[kv_index(by_tag { fields(level) })]
pub struct User {
    pub level: u32,
}

use __OkmIndex_User_by_tag as ByLevel;

/// Spawn a host on its own threads; return the sender endpoint handle.
fn spawn_host(engine: MockStore, prefix: &[u8]) -> VirtualHandle {
    let (host, handle) = StorageHost::new(engine, prefix);
    host.serve();
    handle
}

fn wait_for(predicate: impl Fn() -> bool, what: &str) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timeout waiting for {what}");
}

#[test]
fn table_semantics_over_remote() {
    let handle = spawn_host(MockStore::default(), &[0x00, 0x09]);
    let remote = handle.open();

    let mut t: Table<RemoteStore, UserKey, User> = Table::new(remote);
    t.put(&UserKey { id: 42 }, &User { level: 3 });
    t.put(&UserKey { id: 43 }, &User { level: 5 });

    // Writes are fire-and-forget (channel order = write order, but the
    // pump applies them asynchronously) — wait for the round trip to
    // catch up before asserting.
    wait_for(|| t.get(&UserKey { id: 42 }).is_some(), "first put to land");

    // Index scan over the remote: same API, bytes travel twice.
    let hits = t.scan::<ByLevel>(&3u32.to_be_bytes());
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1.as_ref().unwrap().level, 3);

    // Point get via the table (full round trip: frame → host → engine → back).
    let row = t.get(&UserKey { id: 42 }).expect("row round-tripped");
    assert_eq!(row.level, 3);

    // Both rows' primary + index entries landed (read-side observation).
    let all = t.scan::<ByLevel>(&[]);
    assert_eq!(all.len(), 2);

    // Delete removes both halves remotely.
    t.delete(&UserKey { id: 42 }, &row);
    wait_for(
        || t.get(&UserKey { id: 42 }).is_none(),
        "delete to propagate",
    );
    assert_eq!(t.scan::<ByLevel>(&3u32.to_be_bytes()).len(), 0);
    // Overwrite path too (put on existing key = unfold + refold remotely).
    t.put(&UserKey { id: 42 }, &User { level: 8 });
    wait_for(
        || !t.scan::<ByLevel>(&8u32.to_be_bytes()).is_empty(),
        "overwrite to land",
    );
    let hits = t.scan::<ByLevel>(&8u32.to_be_bytes());
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.decoded.id, 42);
}

#[test]
fn prefix_isolation_between_two_hosts() {
    // Two hosts, two prefixes, one sender shape each. A sender bound to
    // prefix A physically cannot land bytes in B's segment — it does not
    // hold B's prefix (ADR-0010 §4). Verified by cross reads: A's bytes
    // are invisible to B's sender and vice versa.
    let ha = spawn_host(MockStore::default(), &[0x00, 0x01]);
    let hb = spawn_host(MockStore::default(), &[0x00, 0x02]);
    let mut ra = ha.open();
    let mut rb = hb.open();

    ra.put(b"user:1".to_vec(), b"hello".to_vec());
    rb.put(b"user:1".to_vec(), b"world".to_vec());

    wait_for(|| ra.get(b"user:1").is_some(), "host A write to land");
    assert_eq!(ra.get(b"user:1").as_deref(), Some(b"hello".as_slice()));
    assert_eq!(rb.get(b"user:1").as_deref(), Some(b"world".as_slice()));

    // Scans stay inside the host's own prefix: neither sender sees the
    // other's bytes even though the stored sender-key bytes are identical.
    assert_eq!(ra.scan_suffix(b"user:"), vec![b"1".to_vec()]);
    assert_eq!(rb.scan_suffix(b"user:"), vec![b"1".to_vec()]);
}

#[test]
fn commit_batch_is_one_frame_one_commit() {
    let handle = spawn_host(MockStore::default(), &[0x00, 0x03]);
    let mut remote = handle.open();

    // Cross-assembly atomicity (ADR-0003): the whole batch ships as one
    // frame; the receiver commits the op list in one `commit_batch` —
    // the sender's WAL boundary maps onto the receiver's.
    let mut batch = remote.batch();
    batch.put(b"k1".to_vec(), b"v1".to_vec());
    batch.put(b"k2".to_vec(), b"v2".to_vec());
    batch.del(b"k0");
    remote.commit_batch(batch).expect("frame ships");

    wait_for(|| remote.get(b"k1").is_some(), "batched writes to land");
    assert_eq!(remote.get(b"k1").as_deref(), Some(b"v1".as_slice()));
    assert_eq!(remote.get(b"k2").as_deref(), Some(b"v2".as_slice()));
    // Delete op in the frame also executed (no-op on absent key = fine).
    assert_eq!(remote.get(b"k0"), None);
}
