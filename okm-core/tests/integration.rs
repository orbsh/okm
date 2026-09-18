//! 基础集成测试：TestStore 上验证字节布局、双向查询、PrefixKey。
//! fjall 评估见 tests/fjall_eval.rs（--features fjall）。

use okm_core::{DocumentEncode, JunctionEncode, KeyEncode, KvJunction, Ref, TestStore};

// ================= 端点文档（derive DocumentEncode） =================

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

/// 端点文档 A：user（ns=1）
#[derive(DocumentEncode, Clone)]
#[ok_ns(1)]
#[ok_ref(UserKey)]
pub struct User {
    pub org_id: u32,
    pub user_id: u64,
}

/// 端点文档 B：session（ns=2）
#[derive(DocumentEncode, Clone)]
#[ok_ns(2)]
#[ok_ref(SessionKey)]
pub struct Session {
    pub org_id: u32,
    pub session_id: u64,
}

// ================= junction：user → sessions =================
//
// A 端 entry（ns_user）：回答"这个用户挂了哪些 session"
// B 端 entry（ns_session）：回答"这个 session 属于哪个 user"
//
// 字段用 Ref<Doc, Key> 承载端点——derive 反查文档的 NS_PREFIX（ADR-0015/0016），
// ns 只在文档上声明一次。
#[derive(JunctionEncode, Clone)]
#[ok_junction(1)]
pub struct UserToSession {
    #[ok_head(org_id, user_id)]
    pub user: Ref<User, UserKey>,
    pub session: Ref<Session, SessionKey>,
}

#[test]
fn byte_layout_and_queries() {
    let store = TestStore::slatedb_mem();
    let mut edges: okm_core::Junction<TestStore, UserToSession> = okm_core::Junction::new(store);

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
    // A 端 entry 头 = [ns_user 2B BE][slot 2B BE]：ns=1、junction 段 n=1 → [0,1,0x30,1]
    let e = UserToSession {
        user: Ref::ref_key(UserKey { org_id: 7, user_id: 101 }),
        session: Ref::ref_key(SessionKey { org_id: 7, session_id: 1001 }),
    };
    let fk = e.a_side_key();
    // A 端 entry = [ns_user][slot][A·身份][B·全量身份]：头 4B + 12B + 12B。
    // 本端身份在前（扫描前缀 `[ns][slot][A·身份]` 必须能命中）；对端身份是后缀。
    assert_eq!(fk.len(), 4 + 12 + 12);
    assert_eq!(&fk[..4], &[0, 1, 0x30, 2]); // ns=1 原值 + slot 0x3002（A 侧，dir=0）
    assert_eq!(&fk[4..8], &7u32.to_be_bytes()); // user.org_id（本端）
    assert_eq!(&fk[8..16], &101u64.to_be_bytes()); // user.user_id（本端）
    assert_eq!(&fk[16..20], &7u32.to_be_bytes()); // session.org_id（对端）
    assert_eq!(&fk[20..], &1001u64.to_be_bytes()); // session.session_id（对端）

    // B 端 entry 头 = [ns_session 2B][0x30,1]，session 完整在前
    let rk = e.b_side_key();
    // B 端 entry = [ns_session][slot][A·身份（ok_head 截断到 org_id+user_id，全量 12B）]
    // B 端 entry = [ns_session][slot][B·全量身份][A·身份]：session 本端在前
    assert_eq!(rk.len(), 4 + 12 + 12);
    assert_eq!(&rk[..4], &[0, 2, 0x30, 3]); // ns=2 + slot 0x3003（B 侧，dir=1）
    assert_eq!(&rk[4..8], &7u32.to_be_bytes()); // session.org_id（本端）
    assert_eq!(&rk[8..16], &1001u64.to_be_bytes()); // session.session_id（本端）
    assert_eq!(&rk[16..20], &7u32.to_be_bytes()); // user.org_id（对端）
    assert_eq!(&rk[20..], &101u64.to_be_bytes()); // user.user_id（对端）

    // ---------- A 端：user → sessions（decode 回 SessionKey） ----------
    let sessions = u_org1.get_session(&edges);
    assert_eq!(sessions, vec![s1.clone(), s2.clone()]);
    let sessions2 = u_org2.get_session(&edges);
    assert_eq!(sessions2, vec![s3.clone()]);

    // ---------- B 端：session → user（截断身份返回 PrefixKey） ----------
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

/// 段号语义：A/B 两端 entry 的 ns 不同（1 vs 2），同端点对的其它 junction
/// 靠区分号分开；同 ns 内 junction（段 0x3）与索引（段 0x1）条目结构性隔离。
#[test]
fn head_disjointness() {
    // 自反 junction（两端同 ns）时方向位区分两个方向的扫描前缀：
    // n=1 → A 侧 slot 0x3002、B 侧 slot 0x3003（低 bit = dir）。
    // 端点不同的 junction 两端 ns 不同，方向位只是多余的确定性。
    let a_side = [0u8, 1, 0x30, 2];
    let b_side = [0u8, 2, 0x30, 3];
    assert_ne!(a_side[2..4], b_side[2..4], "direction bit separates the two scan prefixes");
    assert_eq!(a_side[..2], [0, 1]);
    // 段号分派：junction(0x3) ≠ index(0x1) ≠ reduce(0x2)——同一 ns 内不串扰。
    assert_ne!(
        okm_core::index::JUNCTION_SLOT_BASE >> 12,
        okm_core::index::DECLARED_SLOT_BASE >> 12
    );
    assert_ne!(
        okm_core::index::REDUCE_SLOT_BASE >> 12,
        okm_core::index::DECLARED_SLOT_BASE >> 12
    );
}
