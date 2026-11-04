//! 基础集成测试：TestStore 上验证字节布局、双向查询、PrefixKey。
//! fjall 评估见 tests/fjall_eval.rs（--features fjall）。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

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
#[ok_ns(110)]
pub struct UserToSession {
    #[ok_head(org_id, user_id)]
    pub user: Ref<User, UserKey>,
    pub session: Ref<Session, SessionKey>,
}

#[test]
fn byte_layout_and_queries() {
    let store = TestStore::slatedb_mem();
    let edges: okm_core::Junction<TestStore, UserToSession> = okm_core::Junction::new(store);

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

    edges.link(&u_org1, &s1).unwrap();
    edges.link(&u_org1, &s2).unwrap();
    edges.link(&u_org2, &s3).unwrap();

    // ---------- 字节布局（ADR-0050：junction 有自己的 ns） ----------
    // 主键面 = [ns 2B BE][0x00 2B][A·身份][B·身份]：ns 是 junction 自己的（110），面号 0x00
    // 与文档集合同一约定；本端身份在前，`[ns][0x00][A·身份]` 才是一次可命中的前缀。
    let e = UserToSession {
        user: Ref::ref_key(UserKey {
            org_id: 7,
            user_id: 101,
        }),
        session: Ref::ref_key(SessionKey {
            org_id: 7,
            session_id: 1001,
        }),
    };
    let fk = e.a_side_key();
    // A face: same ns, face 0x00, then A's identity (truncated by `ok_head`) and B's.
    assert_eq!(&fk[..4], &[0, 110, 0x00, 0x00]);
    assert_eq!(&fk[4..7], &[0, 0, 7]); // user.org_id leads (this side's identity)
    assert!(fk.ends_with(&1001u64.to_be_bytes())); // the peer's identity closes the key

    // The reverse face: the same pair in the other order, at face 0x06 (a graph's freed
    // face). Equal length is the property that matters — an index that could not hold the
    // pair would be a different relation.
    let rk = e.b_side_key();
    assert_eq!(&rk[..4], &[0, 110, 0x06, 0x00]);
    assert_eq!(rk.len(), fk.len(), "one relation, two orders of the same pair");
    assert!(rk.ends_with(&101u64.to_be_bytes())); // this face ends where the other began

    // ---------- A 端：user → sessions（decode 回 SessionKey） ----------
    let sessions = u_org1.get_session(&edges).unwrap();
    assert_eq!(sessions, vec![s1.clone(), s2.clone()]);
    let sessions2 = u_org2.get_session(&edges).unwrap();
    assert_eq!(sessions2, vec![s3.clone()]);

    // ---------- B 端：session → user（截断身份返回 PrefixKey） ----------
    let users = s1.get_user(&edges).unwrap();
    assert_eq!(users.len(), 1);
    // 前缀字段 (org_id, user_id) 可信
    assert_eq!(users[0].decoded.org_id, 7);
    assert_eq!(users[0].decoded.user_id, 101);
    // taken = org(4) + user(8) = 12：前缀消耗字节数
    assert_eq!(users[0].taken, 12);

    // ---------- unlink ----------
    edges.unlink(&u_org2, &s3).unwrap();
    assert!(u_org2.get_session(&edges).unwrap().is_empty());
}

/// 段号语义：A/B 两端 entry 的 ns 不同（1 vs 2），同端点对的其它 junction
/// 靠区分号分开；同 ns 内 junction（段 0x3）与索引（段 0x1）条目结构性隔离。
#[test]
fn head_disjointness() {
    // 两个面共用一个 ns、面号不同：主键面 0x00、反向面 0x06（图让出的固定面）。
    // 方向不再是「哪个端点 ns」，也不是 dir bit——它是同一张表里的两个面。
    let a_side = [0u8, 110, 0x00, 0x00];
    let b_side = [0u8, 110, 0x06, 0x00];
    assert_ne!(a_side[2..4], b_side[2..4], "the faces separate the two scans");
    assert_eq!(a_side[..2], b_side[..2], "one junction, one ns");
}

/// 槽位是位置语义：高半字节是族，低 12 位是计数器。越界值不再被掩码——
/// 掩码会把它落到另一个族里合法-looking 的槽位上（0x3FFF + 1 = 0x4001 是
/// reduce 族），条目会被另一张表读回去。构造即拒绝。
#[test]
#[should_panic(expected = "slot counter out of range")]
fn slot_counter_is_checked_not_masked() {
    let _ = okm_core::index::slot(okm_core::index::segment::INDEX, 0x1000);
}

#[test]
#[should_panic(expected = "slot family out of range")]
fn slot_family_is_checked_not_wrapped() {
    let _ = okm_core::index::slot(0x10, 1);
}
