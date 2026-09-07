//! 基础集成测试：MockStore 上验证字节布局、双向查询、PrefixKey。
//! fjall 评估见 tests/fjall_eval.rs（--features fjall）。

use okm::{EdgeEncode, EdgeTable, KeyEncode, KvEdge, MockStore};

// ================= 端点类型（derive KeyEncode） =================

/// UserKey：org 内的用户。org_id 是"组织前缀"，user_id 才是身份终点
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(1)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

/// SessionKey：org 内的会话
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(2)]
pub struct SessionKey {
    pub org_id: u32,
    pub session_id: u64,
}

// ================= 边：user → sessions =================
//
// user → sessions 方向：用户的"身份"是 (org_id, user_id) 两个字段 → kv_head(org_id, user_id)
// session → user 方向：session 的"身份"是完整 SessionKey
//
// 同一个 edge 的两个方向使用不同宽度的端点身份——这就是"主键随方向变化"的表达。
#[derive(EdgeEncode, Clone)]
#[kv_ns(4)]
pub struct UserToSessionEdge {
    #[kv_head(org_id, user_id)]
    pub user_id: UserKey,
    pub session_id: SessionKey,
}

#[test]
fn byte_layout_and_queries() {
    let store = MockStore::default();
    let mut edges: EdgeTable<MockStore, UserToSessionEdge> = EdgeTable::new(store);

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
    // 头部 2B = (ns<<1 | dir).to_be_bytes()：ns=4、FWD → [0x08, 0x00]
    let e = UserToSessionEdge {
        user_id: u_org1.clone(),
        session_id: s1.clone(),
    };
    let fk = e.forward_key();
    assert_eq!(fk.len(), 2 + 12 + 12);
    assert_eq!(&fk[..2], &[0, 8]); // ns=4<<1|0 → u16 BE，方向位在最低位
    assert_eq!(&fk[2..6], &7u32.to_be_bytes()); // user.org_id
    assert_eq!(&fk[6..14], &101u64.to_be_bytes()); // user.user_id
    assert_eq!(&fk[14..18], &7u32.to_be_bytes()); // session.org_id
    assert_eq!(&fk[18..], &1001u64.to_be_bytes()); // session.session_id

    // reverse: 头部 [0x08, 0x01]，session 完整在前
    let rk = e.reverse_key();
    assert_eq!(rk.len(), 2 + 12 + 12);
    assert_eq!(&rk[..2], &[0, 9]); // ns=4<<1|1
    assert_eq!(&rk[2..6], &7u32.to_be_bytes()); // session.org_id
    assert_eq!(&rk[6..14], &1001u64.to_be_bytes()); // session.session_id
    assert_eq!(&rk[14..18], &7u32.to_be_bytes()); // user.org_id
    assert_eq!(&rk[18..], &101u64.to_be_bytes()); // user.user_id

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
/// 前缀扫描互不串扰——直接用不同 ns 验证隔离。
#[test]
fn head_disjointness() {
    // ns=4 → FWD 头 [0x08,0x00]，REV 头 [0x08,0x01]：共享前缀 [0x08,0x0] 不成立
    // （REV 第二字节 0x01 ≠ FWD 的 0x00），扫描 [0x08,0x00] 不会命中 REV key。
    assert_eq!(okm::head_bytes(4, false), [0, 8]);
    assert_eq!(okm::head_bytes(4, true), [0, 9]);
    assert_eq!(okm::head_bytes(127, false), [0, 254]);
    assert_eq!(okm::head_bytes(127, true), [0, 255]);
}
