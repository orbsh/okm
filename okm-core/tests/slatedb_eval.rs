//! slatedb 真实引擎评估（InMemory object store）。
//! 运行：cargo test --features slatedb

#![cfg(feature = "slatedb")]

use okm_core::slatedb_backend::{AsyncJunction, SlatedbStore};
use okm_core::VirtualStorageAsync; // ADR-0027: commit_batch / scan_range_iter on the async trait
mod common;
use common::*;

#[tokio::test]
async fn slatedb_roundtrip() {
    let store = slatedb::object_store::memory::InMemory::new();
    let db = SlatedbStore::open("okm-test", std::sync::Arc::new(store))
        .await
        .unwrap();
    let edges: AsyncJunction<SlatedbStore, UserToSessionEdge> = AsyncJunction::new(db);

    let u = UserKey {
        org_id: 7,
        user_id: 101,
    };
    let s1 = SessionKey {
        org_id: 7,
        session_id: 1001,
    };
    let s2 = SessionKey {
        org_id: 7,
        session_id: 1002,
    };

    edges.link(&u, &s1).await;
    edges.link(&u, &s2).await;

    let sessions = edges.forward(&u).await;
    assert_eq!(sessions.len(), 2);

    let raw = edges.reverse_raw(&s1).await;
    assert_eq!(raw.len(), 1);

    edges.unlink(&u, &s1).await;
    assert_eq!(edges.forward(&u).await.len(), 1);
}

/// ADR-0027 async-surface alignment: commit_batch takes the native
/// WriteBatch path (ONE db.write), scan_range_iter yields pairs in one
/// pass, and the free suffix-pair scan rides the iter.
#[tokio::test]
async fn slatedb_async_surface_alignment() {
    let store = slatedb::object_store::memory::InMemory::new();
    let mut db = SlatedbStore::open("okm-align", std::sync::Arc::new(store))
        .await
        .unwrap();

    let mut batch = okm_core::MemBatch::default();
    batch.put(b"a/1".to_vec(), b"v1".to_vec());
    batch.put(b"a/2".to_vec(), b"v2".to_vec());
    batch.del(b"a/1"); // a mixed op list commits as ONE frame of work
    db.commit_batch(batch).await.expect("native batch commit");

    assert_eq!(db.get(b"a/1").await, None, "delete op landed");
    assert_eq!(db.get(b"a/2").await.as_deref(), Some(b"v2".as_slice()));

    // pair scan over the prefix: single surviving entry, suffix-relative.
    let pairs = okm_core::scan_suffix_kv_async(&db, b"a/").await;
    assert_eq!(pairs, vec![(b"2".to_vec(), b"v2".to_vec())], "suffix-pair scan");
}
