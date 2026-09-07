//! 二级索引集成测试（ADR-0006 Row 形状）：RowEncode 派生、Table 装配点
//! put/scan 回表、entry 布局 hex 锁定（entry = [ns+slot 2B][索引字段]
//! [key前缀]，value = includes 段）、最左前缀扫描、includes 覆盖、
//! 截断 key 前缀（尾段去冗余，前提：剩余字段已唯一）。

use okm::{KeyEncode, KvEngine, KvIndex, MockStore, PrefixKey, Row, RowEncode};

// marker struct 生成在 derive 展开点（本文件），直接引用
use __OkmIndex_User_by_reputation as ByReputation;
use __OkmIndex_Post_by_timeline as ByTimeline;
use __OkmIndex_Session_by_kind as ByKind;

/// UserKey：代理主键，不含任何属性信息。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(9)]
pub struct UserKey {
    pub id: u64,
}

/// User 行：by_reputation 按 payload 的 reputation 排序，key 前缀取满
/// 主键（默认）。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(UserKey)]
#[kv_index(by_reputation { fields(reputation) })]
pub struct User {
    pub reputation: u32,
}

/// PostKey：代理主键。作者/时间等业务维度是行的属性（payload），
/// 不是身份——身份由代理 id 承担，避免同一信息存两处。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(12)]
pub struct PostKey {
    pub id: u64,
}

/// Post 行：by_timeline 按 (author_id, created_at) 排序——分组维度
/// author_id 是 payload 外键属性，排在 fields 首位，一条
/// `entry_prefix(author_id)` 前缀扫描即返回该作者的整个时间线；
/// includes(title_len) 使扫描无需回表。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(PostKey)]
#[kv_index(by_timeline {
    fields(author_id, created_at),
    includes(title_len),
})]
pub struct Post {
    pub author_id: u64,
    pub created_at: u64,
    pub title_len: u32,
}

/// SessionKey：自然复合键（用户 + 会话），session_id 全局唯一。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(15)]
pub struct SessionKey {
    pub user_id: u64,
    pub session_id: u64,
}

/// Session 行：by_kind 演示截断 key 前缀——fields(kind) 已含分组
/// 维度，尾段只需 session_id（全局唯一）即可区分行，user_id 从尾段
/// 去掉是安全的去冗余（(kind, session_id) 仍行级唯一）。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(SessionKey)]
#[kv_index(by_kind {
    fields(kind),
    key(session_id),
})]
pub struct Session {
    pub kind: u8,
}

fn mk_user(id: u64, reputation: u32) -> (UserKey, User) {
    (UserKey { id }, User { reputation })
}

fn mk_post(id: u64, author: u64, created_at: u64, title_len: u32) -> (PostKey, Post) {
    (
        PostKey { id },
        Post {
            author_id: author,
            created_at,
            title_len,
        },
    )
}

fn mk_session(user: u64, sid: u64, kind: u8) -> (SessionKey, Session) {
    (
        SessionKey {
            user_id: user,
            session_id: sid,
        },
        Session { kind },
    )
}

#[test]
fn table_put_writes_primary_and_indexes() {
    let mut t = <User as Row>::table(MockStore::default(), 9);
    let (k, r) = mk_user(101, 100);
    t.put(&k, &r);

    // 主键写入 slot 0：头 [0,9] + key payload
    let kl = <UserKey as KeyEncode>::KEY_LEN;
    let pk = t.primary_key(&k);
    assert_eq!(&pk[..2], &[0, 9]);
    assert_eq!(&pk[2..], &k.encode()[..]);
    // value = 行载荷 TLV，可解码回原行
    let raw = t.store().get(&pk).unwrap();
    let dec = <User as Row>::decode_payload(&raw);
    assert_eq!(dec, r);

    // 索引 entry：ns = 9+1（by_reputation）
    // entry key 尾部 = 完整主键 id；value = includes 段（无 includes → 空）
    let e1 = t.index_key::<ByReputation>(&k, &r);
    assert_eq!(&e1[..2], &(10u16).to_be_bytes());
    assert_eq!(&e1[e1.len() - kl..], &k.encode()[..]);
    assert!(t.store().get(&e1).is_some());

    // delete：主键 + 全部声明索引一并移除（声明即注册表）
    t.delete(&k, &r);
    assert!(t.store().get(&t.primary_key(&k)).is_none());
    assert!(t.store().get(&t.index_key::<ByReputation>(&k, &r)).is_none());
}

#[test]
fn scan_via_index_returns_rows() {
    let mut t = <User as Row>::table(MockStore::default(), 9);
    let rows_in = [
        mk_user(101, 10),
        mk_user(102, 20),
        mk_user(103, 30),
        mk_user(104, 40),
    ];
    for (k, r) in &rows_in {
        t.put(k, r);
    }

    // 最左前缀 reputation=10 → 1 行回表（key 前缀取满，可完整解码）
    let rows = t.scan::<ByReputation>(&10u32.to_be_bytes());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0.taken, <UserKey as KeyEncode>::KEY_LEN);
    assert_eq!(rows[0].0.decoded, rows_in[0].0);
    assert_eq!(rows[0].1.as_ref().unwrap(), &rows_in[0].1);

    // 空前缀 = 全索引扫描，按索引字段序（10..40）排列
    let all = t.scan::<ByReputation>(&[]);
    assert_eq!(all.len(), 4);
    assert_eq!(all[0].1.as_ref().unwrap(), &rows_in[0].1);
    assert_eq!(all[3].0.decoded.id, 104);
}

#[test]
fn entry_layout_hex_lock() {
    // 直接锁定 by_reputation entry 字节布局：[ns+slot 2B][reputation 4B][id 8B]
    let (k, r) = mk_user(101, 100);
    let kl = <UserKey as KeyEncode>::KEY_LEN; // 8
    assert_eq!(kl, 8);

    let e = ByReputation::entry_key(9, &k, &r);
    assert_eq!(&e[..2], &(10u16).to_be_bytes()); // ns = 9+1（slot 1）
    assert_eq!(&e[2..6], &100u32.to_be_bytes()); // 索引字段 reputation 来自 payload
    assert_eq!(&e[6..], &k.encode()[..]); // key 前缀取满 = 完整主键
    assert_eq!(e.len(), 2 + 4 + kl);
    // value：无 includes → 空
    assert!(ByReputation::entry_value(&k, &r).is_empty());

    // Post.by_timeline：ns = 12+1，索引字段 author_id(8B)+created_at(8B)
    // （均来自 payload），key 前缀取满 id(8B)；value = includes(title_len) 段
    let (pk, pr) = mk_post(500, 7, 1700000000, 42);
    let e2 = ByTimeline::entry_key(12, &pk, &pr);
    assert_eq!(&e2[..2], &(13u16).to_be_bytes());
    assert_eq!(&e2[2..10], &7u64.to_be_bytes()); // author_id 来自 payload
    assert_eq!(&e2[10..18], &1700000000u64.to_be_bytes()); // created_at 来自 payload
    assert_eq!(&e2[18..], &pk.encode()[..]); // key 前缀取满
    assert_eq!(e2.len(), 2 + 8 + 8 + 8);
    assert_eq!(ByTimeline::entry_value(&pk, &pr), 42u32.to_be_bytes().to_vec());

    // Session.by_kind：ns = 15+1，索引字段 kind(1B)，key 前缀截断到
    // session_id(8B)——user_id 从尾段去掉（去冗余）
    let (sk, sr) = mk_session(9, 777, 3);
    let e3 = ByKind::entry_key(15, &sk, &sr);
    assert_eq!(&e3[..2], &(16u16).to_be_bytes());
    assert_eq!(&e3[2..3], &[3]); // kind 来自 payload
    assert_eq!(&e3[3..], &777u64.to_be_bytes()); // 截断尾段 = session_id
    assert_eq!(e3.len(), 2 + 1 + 8);
}

#[test]
fn timeline_is_a_list_encoding() {
    // by_timeline：fields(author_id, created_at)，分组维度在 fields 首位，
    // 一条前缀扫描（author=7）即返回该作者的整个时间线，组内按
    // created_at 升序；includes(title_len) 覆盖，无需回表。
    let mut t = <Post as Row>::table(MockStore::default(), 12);
    let rows = [
        mk_post(500, 7, 30, 10),
        mk_post(501, 7, 20, 11),
        mk_post(502, 7, 10, 12),
        mk_post(503, 8, 40, 13), // 另一位作者，不进作者 7 的列表
    ];
    for (k, r) in &rows {
        t.put(k, r);
    }

    // 原始前缀扫描：author=7 → 3 条 entry。suffix（去掉 ns+author 前缀）
    // = created_at(8B) + 完整主键 id(8B) = 16B。
    let p = ByTimeline::entry_prefix(12, &7u64.to_be_bytes());
    let hits = t.store().scan_suffix(&p);
    assert_eq!(hits.len(), 3);
    for sfx in &hits {
        assert_eq!(sfx.len(), 16);
    }

    // scan API：taken = 8（PostKey 宽度），可完整解码 → 回表
    let listed = t.scan::<ByTimeline>(&7u64.to_be_bytes());
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].0.taken, <PostKey as KeyEncode>::KEY_LEN);
    assert_eq!(listed[0].0.decoded.id, 502); // created_at=10 最先
    assert!(listed[0].1.is_some());

    // scan_covered：includes(title_len) 在 value 里，无需回表即可读；
    // 按 (author, created_at) 升序：10, 20, 30 → title_len 12, 11, 10
    let covered = t.scan_covered::<ByTimeline>(&7u64.to_be_bytes());
    assert_eq!(covered.len(), 3);
    assert_eq!(covered[0].1, 12u32.to_be_bytes().to_vec());
    assert_eq!(covered[2].1, 10u32.to_be_bytes().to_vec());
}

#[test]
fn truncated_key_prefix_drops_redundant_tail() {
    // by_kind：fields(kind) 分组 + key(session_id) 截断尾段。session_id
    // 全局唯一，(kind, session_id) 行级唯一——user_id 留在尾段只会冗余。
    // 扫描 kind=3 → 该类型的全部会话（列表语义），不回表。
    let mut t = <Session as Row>::table(MockStore::default(), 15);
    let rows = [
        mk_session(9, 777, 3),
        mk_session(9, 778, 3),   // 同用户同类型的另一会话：session_id 区分，不覆盖
        mk_session(10, 779, 3),  // 另一用户，同类型
        mk_session(10, 780, 5),  // 另一类型，不进 kind=3 的列表
    ];
    for (k, r) in &rows {
        t.put(k, r);
    }

    let p = ByKind::entry_prefix(15, &[3]);
    let hits = t.store().scan_suffix(&p);
    assert_eq!(hits.len(), 3);
    for sfx in &hits {
        // 尾段只有 8B（session_id），小于完整 KEY_LEN(16B)
        assert_eq!(sfx.len(), 8);
    }

    // scan API：taken = 8（session_id 宽度），decoded.user_id 不可信
    let listed = t.scan::<ByKind>(&[3]);
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].0.taken, 8);
    assert!(listed[0].1.is_none()); // 截断身份 → 不回表

    // 覆盖扫描本例无 includes → value 全空；仅验证条数与顺序
    let covered = t.scan_covered::<ByKind>(&[3]);
    assert_eq!(covered.len(), 3);
    assert!(covered.iter().all(|(_, v)| v.is_empty()));
}

#[test]
fn slot_allocation() {
    // slot 从 1 起：0 保留给主表；ns = table_ns + slot 区分索引
    assert_eq!(<ByReputation as KvIndex>::SLOT, 1);
    assert_eq!(<ByTimeline as KvIndex>::SLOT, 1);
    assert_eq!(<ByKind as KvIndex>::SLOT, 1);
    assert_eq!(<ByReputation as KvIndex>::FIELDS, &["reputation"]);
    assert_eq!(<ByTimeline as KvIndex>::FIELDS, &["author_id", "created_at"]);
    assert_eq!(<ByTimeline as KvIndex>::INCLUDES, &["title_len"]);
    assert_eq!(<ByTimeline as KvIndex>::KEY_PREFIX, &[] as &[&str]);
    assert_eq!(<ByKind as KvIndex>::KEY_PREFIX, &["session_id"]);
    assert_eq!(okm::PRIMARY_SLOT, 0);
    let _ = std::marker::PhantomData::<PrefixKey<UserKey>>;
}

#[test]
fn row_value_tlv_roundtrip() {
    let (_k, r) = mk_user(101, 100);
    let dec = <User as Row>::decode_payload(&r.encode_payload());
    assert_eq!(dec, r);

    let (_pk, pr) = mk_post(500, 7, 1, 2);
    let dec = <Post as Row>::decode_payload(&pr.encode_payload());
    assert_eq!(dec, pr);

    let (_sk, sr) = mk_session(9, 777, 3);
    let dec = <Session as Row>::decode_payload(&sr.encode_payload());
    assert_eq!(dec, sr);
}
