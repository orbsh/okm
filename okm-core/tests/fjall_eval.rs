//! fjall 真实引擎评估：同一条边在磁盘引擎上跑 link/forward/reverse/unlink。
//! 运行：cargo test --features fjall

#![cfg(feature = "fjall")]

use okm_core::{EdgeTable, FjallStore};
mod common;
use common::*;

#[test]
fn fjall_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path(), "edges").unwrap();
    let mut edges: EdgeTable<FjallStore, UserToSessionEdge> = EdgeTable::new(store);

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

    edges.link(&u, &s1);
    edges.link(&u, &s2);

    let sessions = u.get_session(&edges);
    assert_eq!(sessions.len(), 2);

    let users = s1.get_user(&edges);
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].decoded.user_id, 101);

    edges.unlink(&u, &s1);
    assert_eq!(u.get_session(&edges).len(), 1);
    edges.store.persist().unwrap();

    // 重新打开验证持久化（unlink 已生效，剩 1 条）
    drop(edges);
    let store2 = FjallStore::open(dir.path(), "edges").unwrap();
    let edges2: EdgeTable<FjallStore, UserToSessionEdge> = EdgeTable::new(store2);
    let sessions2 = u.get_session(&edges2);
    assert_eq!(sessions2, vec![s2]);
}
