//! 基础集成测试：TestStore 上验证字节布局、双向查询、PrefixKey。
//! fjall 评估见 tests/fjall_eval.rs（--features fjall）。

use okm_core::{EdgeEncode, Edge, KeyEncode, KvEdge, TestStore};

// ================= 端点类型（derive KeyEncode） =================

/// UserKey：org 内的用户。org_id 是"组织前缀"，user_id 才是身份终点
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

/// SessionKey：org 内的会话
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct SessionKey {
    pub org_id: u32,
    pub session_id: u64,
}

// ================= 边：user → sessions =================
//
// user → sessions 方向：用户的"身份"是 (org_id, user_id) 两个字段 → ok_head(org_id, user_id)
// session → user 方向：session 的"身份"是完整 SessionKey
//
// 同一个 edge 的两个方向使用不同宽度的端点身份——这就是"主键随方向变化"的表达。
#[derive(EdgeEncode, Clone)]
#[ok_ns(4)]
pub struct UserToSessionEdge {
    #[ok_head(org_id, user_id)]
    pub user_id: UserKey,
    pub session_id: SessionKey,
}

#[test]
fn byte_layout_and_queries() {
    let store = TestStore::slatedb_mem();
    let mut edges: Edge<TestStore, UserToSessionEdge> = Edge::new(store);

    let u_org1 = UserKey {
        org_id: 7,
        user_id: 101,
    };
    let u_org2 = UserKey {
        org_id: 8,
        user_id: 101,
    }; // 同 user_id 不同 org
    let s1 = SessionKey {
        org_id: 7,
        session_id: 1001,
    };
    let s2 = SessionKey {
        org_id: 7,
        session_id: 1002,
    };
    let s3 = SessionKey {
        org_id: 8,
        session_id: 2001,
    };

    edges.link(&u_org1, &s1);
    edges.link(&u_org1, &s2);
    edges.link(&u_org2, &s3);

    // ---------- 字节布局 ----------
    // 头部 3B = [ns 2B BE][slot 1B]：ns=4、FWD → [0, 4, 14]（PLAN Phase 10）
    let e = UserToSessionEdge {
        user_id: u_org1.clone(),
        session_id: s1.clone(),
    };
    let fk = e.forward_key();
    assert_eq!(fk.len(), 3 + 12 + 12);
    assert_eq!(&fk[..3], &[0, 4, 14]); // ns=4 原值 + EDGE_FWD_SLOT
    assert_eq!(&fk[3..7], &7u32.to_be_bytes()); // user.org_id
    assert_eq!(&fk[7..15], &101u64.to_be_bytes()); // user.user_id
    assert_eq!(&fk[15..19], &7u32.to_be_bytes()); // session.org_id
    assert_eq!(&fk[19..], &1001u64.to_be_bytes()); // session.session_id

    // reverse: 头部 [0, 4, 15]，session 完整在前
    let rk = e.reverse_key();
    assert_eq!(rk.len(), 3 + 12 + 12);
    assert_eq!(&rk[..3], &[0, 4, 15]); // EDGE_REV_SLOT
    assert_eq!(&rk[3..7], &7u32.to_be_bytes()); // session.org_id
    assert_eq!(&rk[7..15], &1001u64.to_be_bytes()); // session.session_id
    assert_eq!(&rk[15..19], &7u32.to_be_bytes()); // user.org_id
    assert_eq!(&rk[19..], &101u64.to_be_bytes()); // user.user_id

    // ---------- 正向：user → sessions（decode 回 SessionKey） ----------
    let sessions = u_org1.get_session(&edges);
    assert_eq!(sessions, vec![s1.clone(), s2.clone()]);
    let sessions2 = u_org2.get_session(&edges);
    assert_eq!(sessions2, vec![s3.clone()]);

    // ---------- 反向：session → user（截断身份返回 PrefixKey） ----------
    let users = s1.get_user(&edges);
    assert_eq!(users.len(), 1);
    // 前缀字段 (org_id, user_id) 可信
    assert_eq!(users[0].decoded.org_id, 7);
    assert_eq!(users[0].decoded.user_id, 101);
    // taken = org(4) + user(8) = 12：前缀消耗字节数
    assert_eq!(users[0].taken, 12);

    // ---------- unlink ----------
    edges.unlink(&u_org2, &s3);
    assert!(u_org2.get_session(&edges).is_empty());
}

/// 方向位语义：同 ns 的 FWD/REV 头部区间严格不相交（ns<<1 vs ns<<1|1），
/// 前缀扫描互不串扰——同 ns 下方向由 slot 区分（PLAN Phase 10）。
#[test]
fn head_disjointness() {
    // ns=4：FWD 头 [0,4,14]，REV 头 [0,4,15]。共享前缀 [0,4] 相同，但
    // slot 字节 14 ≠ 15，扫描 [0,4,14] 不会命中 REV key。
    let mut fwd = 4u16.to_be_bytes().to_vec();
    fwd.push(okm_core::index::EDGE_FWD_SLOT);
    let mut rev = 4u16.to_be_bytes().to_vec();
    rev.push(okm_core::index::EDGE_REV_SLOT);
    assert_eq!(fwd, [0, 4, 14]);
    assert_eq!(rev, [0, 4, 15]);
    assert_eq!(fwd[..2], rev[..2], "same ns segment");
    assert_ne!(fwd[2], rev[2], "slot byte separates directions");
}
