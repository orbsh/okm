//! slatedb 真实引擎评估（InMemory object store）。
//! 运行：cargo test --features slatedb

#![cfg(feature = "slatedb")]

use okm::slatedb_backend::{AsyncEdgeTable, SlatedbStore};
mod common;
use common::*;

#[tokio::test]
async fn slatedb_roundtrip() {
    let store = slatedb::object_store::memory::InMemory::new();
    let db = SlatedbStore::open("okm-test", std::sync::Arc::new(store))
        .await
        .unwrap();
    let edges: AsyncEdgeTable<SlatedbStore, UserToSessionEdge> = AsyncEdgeTable::new(db);

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
