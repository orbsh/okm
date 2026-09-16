//! Engine-matrix verification: every backend in `TestStore::matrix()`
//! (slatedb-mem / fjall / redb, per feature flags) must produce
//! identical behavior for the core operations — key round trips,
//! prefix scans, batch atomicity, and SharedVirtualStorage sharing.

use okm_core::{KeyEncode, KvBatch, ObjEncode, Table, TestStore, VirtualStorage};

fn verify_engine(store: TestStore) {
    let name = store.name();

    // Point ops.
    let mut s = store.clone();
    s.put(b"k1".to_vec(), b"v1".to_vec());
    s.put(b"k2".to_vec(), b"v2".to_vec());
    assert_eq!(s.get(b"k1").as_deref(), Some(b"v1".as_slice()), "{name}: point get");
    assert_eq!(s.get(b"missing"), None, "{name}: absent key");

    // Prefix scan: suffixes in key order.
    let sfx = s.scan_suffix(b"k");
    assert_eq!(sfx, vec![b"1".to_vec(), b"2".to_vec()], "{name}: scan_suffix");

    // Handle sharing: a clone sees the same keyspace.
    let s2 = store.shared_handle();
    assert_eq!(s2.get(b"k1").as_deref(), Some(b"v1".as_slice()), "{name}: shared handle");

    // Batch atomicity: 100 ops in ONE commit_batch.
    let mut batch = s.batch();
    for i in 0..100u64 {
        batch.put(format!("batch:{i}").into_bytes(), b"ok".to_vec());
    }
    batch.del(b"k1");
    s.commit_batch(batch).expect("{name}: batch commit");
    assert_eq!(s.get(b"batch:50").as_deref(), Some(b"ok".as_slice()), "{name}: batch write");
    assert_eq!(s.get(b"k1"), None, "{name}: batch delete");
    assert_eq!(s.scan_suffix(b"batch:").len(), 100, "{name}: all batch entries");

    // Batch failure handling: empty batch commits clean.
    let empty = s.batch();
    s.commit_batch(empty).expect("{name}: empty batch");
}

#[test]
fn engine_matrix_core_ops() {
    for (name, store) in TestStore::matrix() {
        let _ = name;
        verify_engine(store);
    }
}

/// Typed Table over each engine: the compose-key put/get path.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct MKey {
    pub id: u64,
}

#[derive(ObjEncode, Clone, PartialEq, Debug)]
#[ok_ref(MKey)]
#[ok_ns(3)]
pub struct MRow {
    pub score: u32,
}

#[test]
fn engine_matrix_table_round_trip() {
    for (name, store) in TestStore::matrix() {
        let mut t: okm_core::Table<TestStore, MKey, MRow> = okm_core::Table::new(store);
        t.put(&MKey { id: 7 }, &MRow { score: 55 });
        let back = t.get(&MKey { id: 7 }).expect("{name}: row round trip");
        assert_eq!(back.score, 55, "{name}");
        let keys = t.scan_keys();
        assert_eq!(keys, vec![MKey { id: 7 }], "{name}: scan_keys");
    }
}
