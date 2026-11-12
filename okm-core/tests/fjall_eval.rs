//! fjall 真实引擎评估：同一条边在磁盘引擎上跑 link/forward/reverse/unlink。
//! 运行：cargo test --features fjall

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]
#![cfg(feature = "fjall")]

use okm_core::FjallStore;
mod common;
use common::*;

#[test]
fn fjall_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path(), "edges").unwrap();
    let rows: okm_core::Collection<FjallStore, UserSessionKey, UserSessionRow> =
        okm_core::Collection::new(store);

    let k = |user_id: u64, session_id: u64| UserSessionKey {
        user_id,
        session_id,
    };
    rows.put(&k(101, 1001), &UserSessionRow { session_id: 1001 })
        .unwrap();
    rows.put(&k(101, 1002), &UserSessionRow { session_id: 1002 })
        .unwrap();

    // 正向：主键第一列是 user_id
    let probe = okm_core::PrefixProbe::<UserSessionKey>::new(101u64.to_be_bytes().to_vec());
    assert_eq!(rows.scan_keys(&probe).count(), 2);

    // 反向：by_session 索引，一次前缀扫描
    let hits = rows
        .scan::<__OkmIndex_UserSessionRow_by_session>(&1001u64.to_be_bytes())
        .collect::<okm_core::error::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1.decoded.user_id, 101);

    rows.purge(&k(101, 1001)).unwrap();
    assert_eq!(
        rows.scan::<__OkmIndex_UserSessionRow_by_session>(&1001u64.to_be_bytes())
            .collect::<okm_core::error::Result<Vec<_>>>()
            .unwrap()
            .len(),
        0,
        "the index entry left with the row"
    );
    rows.store().persist().unwrap();

    // 重新打开：只有提交过的状态还在
    drop(rows);
    let store2 = FjallStore::open(dir.path(), "edges").unwrap();
    let rows2: okm_core::Collection<FjallStore, UserSessionKey, UserSessionRow> =
        okm_core::Collection::new(store2);
    let probe = okm_core::PrefixProbe::<UserSessionKey>::new(101u64.to_be_bytes().to_vec());
    assert_eq!(rows2.scan_keys(&probe).count(), 1);
}
