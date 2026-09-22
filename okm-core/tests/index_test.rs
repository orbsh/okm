//! 二级索引集成测试（ADR-0006 Document 形状）：DocumentEncode 派生、Collection 装配点
//! put/scan 回表、entry 布局 hex 锁定（entry = [ns 2B][slot 2B][索引字段]
//! [key前缀]，value = includes 段）、最左前缀扫描、includes 覆盖、
//! 截断 key 前缀（尾段去冗余，前提：剩余字段已唯一）。

use okm_core::{KeyEncode, VirtualStorage, KvIndex, TestStore, PrefixKey, Document, DocumentEncode};

// marker struct 生成在 derive 展开点（本文件），直接引用
use __OkmIndex_User_by_reputation as ByReputation;
use __OkmIndex_Post_by_timeline as ByTimeline;
use __OkmIndex_Session_by_kind as ByKind;

/// UserKey：代理主键，不含任何属性信息。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub id: u64,
}

/// User 行：by_reputation 按 payload 的 reputation 排序，key 前缀取满
/// 主键（默认）。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_index(by_reputation { fields(reputation) })]
#[ok_ns(9)]
pub struct User {
    pub reputation: u32,
}

/// PostKey：代理主键。作者/时间等业务维度是行的属性（payload），
/// 不是身份——身份由代理 id 承担，避免同一信息存两处。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PostKey {
    pub id: u64,
}

/// Post 行：by_timeline 按 (author_id, created_at) 排序——分组维度
/// author_id 是 payload 外键属性，排在 fields 首位，一条
/// `entry_prefix(author_id)` 前缀扫描即返回该作者的整个时间线；
/// includes(title_len) 使扫描无需回表。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]
#[ok_index(by_timeline {
    fields(author_id, created_at),
    includes(title_len),
})]
#[ok_ns(12)]
pub struct Post {
    pub author_id: u64,
    pub created_at: u64,
    pub title_len: u32,
}

/// SessionKey：自然复合键（用户 + 会话），session_id 全局唯一。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct SessionKey {
    pub user_id: u64,
    pub session_id: u64,
}

/// Session 行：by_kind 演示截断 key 前缀——fields(kind) 已含分组
/// 维度，尾段只需 session_id（全局唯一）即可区分行，user_id 从尾段
/// 去掉是安全的去冗余（(kind, session_id) 仍行级唯一）。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(SessionKey)]
#[ok_index(by_kind {
    fields(kind),
    key(session_id),
})]
#[ok_ns(15)]
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
    let mut t = <User as Document>::collection(TestStore::slatedb_mem());
    let (k, r) = mk_user(101, 100);
    t.put(&k, &r);

    // 主键写入 slot 0x0000：头 [0,9,0,0]（ns 2B + slot 2B BE）+ key payload
    let kl = <UserKey as KeyEncode>::KEY_LEN;
    let pk = t.primary_key(&k);
    assert_eq!(&pk[..4], &[0, 9, 0, 0]);
    assert_eq!(&pk[4..], &k.encode()[..]);
    // value = 行载荷 TLV，可解码回原行
    let raw = t.store().get(&pk).unwrap();
    let dec = <User as Document>::decode_payload(&raw);
    assert_eq!(dec, r);

    // 索引 entry：ns 9 + slot 0x1001（by_reputation，index 段计数 1）
    // entry key 尾部 = 完整主键 id；value = includes 段（无 includes → 空）
    let e1 = t.index_key::<ByReputation>(&k, &r);
    assert_eq!(&e1[..4], &[0, 9, 0x10, 0x01]); // ns=9、slot=0x1001
    assert_eq!(&e1[e1.len() - kl..], &k.encode()[..]);
    assert!(t.store().get(&e1).is_some());

    // delete：主键 + 全部声明索引一并移除（声明即注册表）
    t.delete(&k, &r);
    assert!(t.store().get(&t.primary_key(&k)).is_none());
    assert!(t.store().get(&t.index_key::<ByReputation>(&k, &r)).is_none());
}

#[test]
fn delete_by_pkey_fetches_row_and_cleans_indexes() {
    let mut t = <User as Document>::collection(TestStore::slatedb_mem());
    let (k, r) = mk_user(101, 100);
    t.put(&k, &r);

    // 只给主键：内部 get 回 document，两半同源派生
    assert!(t.delete_by_pkey(&k));
    assert!(t.store().get(&t.primary_key(&k)).is_none());
    assert!(t.store().get(&t.index_key::<ByReputation>(&k, &r)).is_none());

    // 不存在的主键：no-op，返回 false
    let (missing, _) = mk_user(999, 1);
    assert!(!t.delete_by_pkey(&missing));
}

#[test]
fn scan_via_index_returns_rows() {
    let mut t = <User as Document>::collection(TestStore::slatedb_mem());
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
    // 直接锁定 by_reputation entry 字节布局：[ns 2B][slot 2B][reputation 4B][id 8B]
    let (k, r) = mk_user(101, 100);
    let kl = <UserKey as KeyEncode>::KEY_LEN; // 8
    assert_eq!(kl, 8);

    let e = ByReputation::entry_key(<User as Document>::NS_PREFIX, &k, &r);
    assert_eq!(&e[..4], &[0, 9, 0x10, 0x01]); // ns=9、slot=0x1001
    assert_eq!(&e[4..8], &100u32.to_be_bytes()); // 索引字段 reputation 来自 payload
    assert_eq!(&e[8..], &k.encode()[..]); // key 前缀取满 = 完整主键
    assert_eq!(e.len(), 4 + 4 + kl);
    // value：无 includes → 空
    assert!(ByReputation::entry_value(&k, &r).is_empty());

    // Post.by_timeline：ns 12 + slot 0x1001，索引字段 author_id(8B)+created_at(8B)
    // （均来自 payload），key 前缀取满 id(8B)；value = includes(title_len) 段
    let (pk, pr) = mk_post(500, 7, 1700000000, 42);
    let e2 = ByTimeline::entry_key(<Post as Document>::NS_PREFIX, &pk, &pr);
    assert_eq!(&e2[..4], &[0, 12, 0x10, 0x01]); // ns=12、slot=0x1001
    assert_eq!(&e2[4..12], &7u64.to_be_bytes()); // author_id 来自 payload
    assert_eq!(&e2[12..20], &1700000000u64.to_be_bytes()); // created_at 来自 payload
    assert_eq!(&e2[20..], &pk.encode()[..]); // key 前缀取满
    assert_eq!(e2.len(), 4 + 8 + 8 + 8);
    assert_eq!(ByTimeline::entry_value(&pk, &pr), 42u32.to_be_bytes().to_vec());

    // Session.by_kind：ns = 15+1，索引字段 kind(1B)，key 前缀截断到
    // session_id(8B)——user_id 从尾段去掉（去冗余）
    let (sk, sr) = mk_session(9, 777, 3);
    let e3 = ByKind::entry_key(<Session as Document>::NS_PREFIX, &sk, &sr);
    assert_eq!(&e3[..4], &[0, 15, 0x10, 0x01]); // ns=15、slot=0x1001
    assert_eq!(&e3[4..5], &[3]); // kind 来自 payload
    assert_eq!(&e3[5..], &777u64.to_be_bytes()); // 截断尾段 = session_id
    assert_eq!(e3.len(), 4 + 1 + 8);
}

#[test]
fn timeline_is_a_list_encoding() {
    // by_timeline：fields(author_id, created_at)，分组维度在 fields 首位，
    // 一条前缀扫描（author=7）即返回该作者的整个时间线，组内按
    // created_at 升序；includes(title_len) 覆盖，无需回表。
    let mut t = <Post as Document>::collection(TestStore::slatedb_mem());
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
    let p = ByTimeline::entry_prefix(<Post as Document>::NS_PREFIX, &7u64.to_be_bytes());
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
    let mut t = <Session as Document>::collection(TestStore::slatedb_mem());
    let rows = [
        mk_session(9, 777, 3),
        mk_session(9, 778, 3),   // 同用户同类型的另一会话：session_id 区分，不覆盖
        mk_session(10, 779, 3),  // 另一用户，同类型
        mk_session(10, 780, 5),  // 另一类型，不进 kind=3 的列表
    ];
    for (k, r) in &rows {
        t.put(k, r);
    }

    let p = ByKind::entry_prefix(<Session as Document>::NS_PREFIX, &[3]);
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
    // slot 段号制（ADR-0016）：索引段 0x1，计数从 1 起；段号 = slot >> 12，
    // 在集合 ns 段内结构化区分条目种类
    assert_eq!(<ByReputation as KvIndex>::SLOT, 0x1001);
    assert_eq!(<ByTimeline as KvIndex>::SLOT, 0x1001);
    assert_eq!(<ByKind as KvIndex>::SLOT, 0x1001);
    assert_eq!(<ByReputation as KvIndex>::FIELDS, &["reputation"]);
    assert_eq!(<ByTimeline as KvIndex>::FIELDS, &["author_id", "created_at"]);
    assert_eq!(<ByTimeline as KvIndex>::INCLUDES, &["title_len"]);
    assert_eq!(<ByTimeline as KvIndex>::KEY_PREFIX, &[] as &[&str]);
    assert_eq!(<ByKind as KvIndex>::KEY_PREFIX, &["session_id"]);
    assert_eq!(okm_core::PRIMARY_SLOT, 0x0000);
    let _ = std::marker::PhantomData::<PrefixKey<UserKey>>;
}

#[test]
fn row_value_tlv_roundtrip() {
    let (_k, r) = mk_user(101, 100);
    let dec = <User as Document>::decode_payload(&r.encode_payload());
    assert_eq!(dec, r);

    let (_pk, pr) = mk_post(500, 7, 1, 2);
    let dec = <Post as Document>::decode_payload(&pr.encode_payload());
    assert_eq!(dec, pr);

    let (_sk, sr) = mk_session(9, 777, 3);
    let dec = <Session as Document>::decode_payload(&sr.encode_payload());
    assert_eq!(dec, sr);
}

/// DocKey：代理主键。name 是变长 payload 字段（text-first 索引 regime），
/// city 是分组维度（定宽，居首可定位）。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct DocKey {
    pub id: u64,
}

/// Doc 行：by_city_name 复合索引 = (city, name)，变长 name 居末；
/// by_name 单字段文本索引。排序 = 字典序（裸 UTF-8 字节，无长度前缀——
/// 长度前缀会先按长度后按字节，摧毁字典序）。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DocKey)]
#[ok_index(
    by_city_name {
        fields(city, name),
    },
    by_name { fields(name) },
)]
#[ok_ns(21)]
pub struct Doc {
    pub city: u32,
    pub name: String,
}

use __OkmIndex_Doc_by_city_name as ByCityName;
use __OkmIndex_Doc_by_name as ByName;

fn mk_doc(id: u64, city: u32, name: &str) -> (DocKey, Doc) {
    (DocKey { id }, Doc { city, name: name.to_string() })
}

#[test]
fn variable_length_index_text_first() {
    // 变长字段索引（text-first regime）：裸 UTF-8 字节参与排序，共享前缀
    // 文本按字典序相邻；精确匹配靠尾部主键回表核验（无定界符是本 regime
    // 的代价——"ab" 的前缀扫描会扫到 "abc"，这正是字典序的行为）。
    let mut t = <Doc as Document>::collection(TestStore::slatedb_mem());
    let rows = [
        mk_doc(1, 10, "alpha"),
        mk_doc(2, 10, "alphabet"),   // "alpha" 的扩展，字典序紧随其后
        mk_doc(3, 10, "beta"),
        mk_doc(4, 20, "alpha"),      // 同名不同城市，city 维度隔开
    ];
    for (k, r) in &rows {
        t.put(k, r);
    }

    // entry 布局：[ns 2B][slot 2B][city 4B][name 裸 UTF-8][id 8B]
    // value：无 includes → 空
    let kl = <DocKey as KeyEncode>::KEY_LEN;
    assert_eq!(kl, 8);
    let (k, r) = &rows[0];
    let e = ByCityName::entry_key(<Doc as Document>::NS_PREFIX, k, r);
    assert_eq!(&e[..4], &[0, 21, 0x10, 0x01]); // ns=21、slot=0x1001
    assert_eq!(&e[4..8], &10u32.to_be_bytes()); // city 定宽可定位
    assert_eq!(&e[8..13], b"alpha"); // name 居末：裸字节，无长度前缀
    assert_eq!(&e[13..], &k.encode()[..]); // 尾部主键干净切出
    assert_eq!(e.len(), 4 + 4 + 5 + kl);

    // 前缀扫描 "alpha" 命中 alpha + alphabet（共享前缀 = 同一字典序区间）
    let p = ByCityName::entry_prefix(<Doc as Document>::NS_PREFIX, &10u32.to_be_bytes());
    let hits = t.store().scan_suffix(&p);
    assert_eq!(hits.len(), 3); // city=10 的 3 行

    // scan API：name="beta"，回表核验后返回行
    let listed = t.scan::<ByCityName>(
        &[&10u32.to_be_bytes()[..], b"beta"].concat(),
    );
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0.decoded.id, 3);
    assert_eq!(listed[0].1.as_ref().unwrap(), &rows[2].1);

    // by_name：单字段文本索引，空前缀 = 全表按 name 字典序
    let all = t.scan::<ByName>(&[]);
    let names: Vec<&str> = all.iter().map(|(_, r)| r.as_ref().unwrap().name.as_str()).collect();
    assert_eq!(names, ["alpha", "alpha", "alphabet", "beta"]);
    // 精确匹配："alpha" 扫出 2 行（含 alphabet），去重靠主键
    let alphas = t.scan::<ByName>(b"alpha");
    assert_eq!(alphas.len(), 3);
}

/// 函数索引（function-index regime）：归一化函数同时驱动写入端编码与
/// 查询端探针——一条声明，两侧共用。
fn lower_name(document: &DocFunc) -> String {
    document.name.to_lowercase()
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DocKey)]
#[ok_index(by_lower { func(lower_name) })]
#[ok_ns(21)]
pub struct DocFunc {
    pub city: u32,
    pub name: String,
}

use __OkmIndex_DocFunc_by_lower as ByLower;

fn mk_docf(id: u64, city: u32, name: &str) -> (DocKey, DocFunc) {
    (DocKey { id }, DocFunc { city, name: name.to_string() })
}

#[test]
fn function_index_normalizes_both_sides() {
    // 写入端：entry 排序段 = lower_name(&document) 的结果（裸 UTF-8，字典序）；
    // 查询端：探针行调用同一个函数归一化，两侧共享一条声明。
    let mut t = <DocFunc as Document>::collection(TestStore::slatedb_mem());
    let rows = [
        mk_docf(1, 10, "Apple"),
        mk_docf(2, 10, "APPLE"),
        mk_docf(3, 10, "banana"),
    ];
    for (k, r) in &rows {
        t.put(k, r);
    }

    // entry 布局：[ns 2B][slot 2B][func 结果裸字节][id 8B]——无 fields 段
    let kl = <DocKey as KeyEncode>::KEY_LEN;
    let (k, r) = &rows[0];
    let e = ByLower::entry_key(<DocFunc as Document>::NS_PREFIX, k, r);
    assert_eq!(&e[..4], &[0, 21, 0x10, 0x01]); // ns=21、slot=0x1001
    assert_eq!(&e[4..9], b"apple"); // 归一化后的结果
    assert_eq!(&e[9..], &k.encode()[..]);
    assert_eq!(e.len(), 4 + 5 + kl);
    assert_eq!(ByLower::FUNC, "lower_name");

    // 查询端探针：同一函数归一化 probe 行 → 前缀扫描 → 回表
    let probe = DocFunc { city: 0, name: "APPLE".to_string() };
    let listed = t.scan::<ByLower>(probe.name.to_lowercase().as_bytes());
    assert_eq!(listed.len(), 2); // Apple + APPLE 归一化到同一区间
    let ids: Vec<u64> = listed.iter().map(|(pk, _)| pk.decoded.id).collect();
    assert_eq!(ids, [1, 2]); // 字典序（同结果 → 主键序）

    // delete：函数索引 entry 一并移除
    t.delete(k, r);
    assert!(t.store().get(&e).is_none());
}

#[test]
fn multi_entry_function_index_fans_out() {
    // 多值函数索引：func 返回 Vec<String>，一行 fan out 成 N 条 entry
    // （倒排索引形态：token → 该 token 下的主键集合）。读路径复用
    // 既有 scan::<I>——token 就是数据段，主键前缀从尾部反推。
    fn tokens(document: &DocTags) -> Vec<String> {
        document.tags.split(',').map(|s| s.to_string()).collect()
    }

    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(DocKey)]
    #[ok_ns(21)]
    #[ok_index(by_tag { func(tokens) })]
    pub struct DocTags {
        pub tags: String,
    }

    use __OkmIndex_DocTags_by_tag as ByTag;

    let mut t = <DocTags as Document>::collection(TestStore::slatedb_mem());
    let rows = [
        (DocKey { id: 1 }, DocTags { tags: "rust,kv".into() }),
        (DocKey { id: 2 }, DocTags { tags: "rust,storage".into() }),
    ];
    for (k, r) in &rows {
        t.put(k, r);
    }

    // 一行两条 entry：token 居数据段（变长，贴主键前），主键 8B 从尾部切出
    let e = ByTag::entry_pairs(<DocTags as Document>::NS_PREFIX, &rows[0].0, &rows[0].1);
    assert_eq!(e.len(), 2);
    let kl = <DocKey as KeyEncode>::KEY_LEN;
    let mut toks: Vec<&[u8]> =
        e.iter().map(|(k, _)| &k[4..k.len() - kl]).collect();
    toks.sort();
    assert_eq!(toks, vec![b"kv".as_slice(), b"rust".as_slice()]);

    // scan 语义：按 token 前缀扫 → 回表（返回行）
    let rust_rows = t.scan::<ByTag>(b"rust");
    assert_eq!(rust_rows.len(), 2); // 两行都打了 rust

    let kv_rows = t.scan::<ByTag>(b"kv");
    assert_eq!(kv_rows.len(), 1);
    assert_eq!(kv_rows[0].0.decoded.id, 1);

    // 删除：delete 沿 entry_pairs 逐条清除，无悬挂条目
    t.delete(&rows[0].0, &rows[0].1);
    let remaining = t.scan::<ByTag>(b"rust").len();
    assert_eq!(remaining, 1); // 只剩 id 2
}

// ---------------------------------------------------------------------------
// 部分索引（partial index）：where(path) 行级谓词
// ---------------------------------------------------------------------------

/// TicketKey：代理主键。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct TicketKey {
    pub id: u64,
}

/// 谓词收整行，读哪些列由业务决定——这里读的是不参与排序的 status
/// （非索引列同样可以）。
fn ticket_is_open(document: &Ticket) -> bool {
    document.status == 0
}

/// 函数索引形态的等价过滤：空 Vec = 该行不产生任何 entry（无 where）。
/// 与 where 的分工——where 管行级条件（声明可见），函数体内过滤管
/// 逐值丢弃（某个 token 不要，其它保留）。
fn live_tokens(document: &Ticket) -> Vec<String> {
    if document.status != 0 {
        return Vec::new();
    }
    document.tags.split(',').map(|s| s.to_string()).collect()
}

/// Ticket：open_by_assignee 是「常规字段索引 + 覆盖段 + 谓词」三者叠加
/// （只索引未关闭工单是常见分布——存量里 99% 已关闭）；by_live_token 是
/// 函数索引形态的同一条件，靠空 Vec 表达。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(TicketKey)]
#[ok_index(open_by_assignee {
    fields(assignee_id, created_at),
    includes(title_len),
    where(ticket_is_open),
})]
#[ok_index(by_live_token { func(live_tokens) })]
#[ok_index(by_open_token { func(all_tokens), where(ticket_is_open) })]
#[ok_ns(22)]
pub struct Ticket {
    pub assignee_id: u64,
    pub created_at: u64,
    pub title_len: u32,
    /// 0 = open，1 = closed——谓词读的非索引列
    pub status: u8,
    pub tags: String,
}

use __OkmIndex_Ticket_open_by_assignee as ByOpenAssignee;
use __OkmIndex_Ticket_by_live_token as ByLiveToken;
use __OkmIndex_Ticket_by_open_token as ByOpenToken;

/// 无条件的 token 切分——过滤交给声明上的 where（函数索引形态的谓词）。
fn all_tokens(document: &Ticket) -> Vec<String> {
    document.tags.split(',').map(|s| s.to_string()).collect()
}

fn mk_ticket(
    id: u64,
    assignee: u64,
    created_at: u64,
    title_len: u32,
    status: u8,
    tags: &str,
) -> (TicketKey, Ticket) {
    (
        TicketKey { id },
        Ticket {
            assignee_id: assignee,
            created_at,
            title_len,
            status,
            tags: tags.to_string(),
        },
    )
}

#[test]
fn partial_index_admits_only_matching_rows() {
    let mut t = <Ticket as Document>::collection(TestStore::slatedb_mem());
    let open_a = mk_ticket(1, 7, 10, 3, 0, "");
    let closed = mk_ticket(2, 7, 20, 5, 1, "");
    let open_b = mk_ticket(3, 7, 30, 2, 0, "");
    for (k, r) in [&open_a, &closed, &open_b] {
        t.put(k, r);
    }

    // 谓词在 entry_pairs 里短路：被拒的行主表条目在、索引条目不在。
    // entry key 仍可算（它是行的派生地址），只是写路径没写它。
    let e_closed = t.index_key::<ByOpenAssignee>(&closed.0, &closed.1);
    assert!(t.store().get(&t.primary_key(&closed.0)).is_some());
    assert!(t.store().get(&e_closed).is_none());
    assert!(!ByOpenAssignee::admits(&closed.1));
    assert!(ByOpenAssignee::admits(&open_a.1));

    // 扫描只返回被接纳的行，组内按 created_at（10, 30）
    let rows = t.scan::<ByOpenAssignee>(&7u64.to_be_bytes());
    let ids: Vec<u64> = rows.iter().map(|(pk, _)| pk.decoded.id).collect();
    assert_eq!(ids, [1, 3]);
    // 整索引扫描（空前缀）同样只有 2 条——稀疏性对读侧透明
    assert_eq!(t.scan::<ByOpenAssignee>(&[]).len(), 2);

    // 覆盖扫描不受谓词影响：includes(title_len) 仍在 value 里
    let covered = t.scan_covered::<ByOpenAssignee>(&7u64.to_be_bytes());
    assert_eq!(covered.len(), 2);
    assert_eq!(covered[0].1, 3u32.to_be_bytes().to_vec());
    assert_eq!(covered[1].1, 2u32.to_be_bytes().to_vec());

    // delete 走同一个 entry_pairs：被接纳的行清干净
    t.delete(&open_a.0, &open_a.1);
    assert!(t.store().get(&t.index_key::<ByOpenAssignee>(&open_a.0, &open_a.1)).is_none());
    assert_eq!(t.scan::<ByOpenAssignee>(&7u64.to_be_bytes()).len(), 1);
}

#[test]
fn partial_index_flip_needs_delete_then_put() {
    // 谓词翻转（open → closed）：entry key 只由索引字段决定，谓词不进 key
    // ——同一个地址，写侧只是这次不再产它。put 覆盖不清理旧索引条目
    // （document.rs 的既有契约，值变化与谓词翻转同类），于是旧条目悬挂，
    // 且扫描把它当被接纳的行返回——谓词的保证到 delete 为止。
    let mut t = <Ticket as Document>::collection(TestStore::slatedb_mem());
    let (k, open) = mk_ticket(1, 7, 10, 3, 0, "");
    t.put(&k, &open);
    let e = t.index_key::<ByOpenAssignee>(&k, &open);

    let closed = Ticket { status: 1, ..open.clone() };
    t.put(&k, &closed);
    assert_eq!(e, t.index_key::<ByOpenAssignee>(&k, &closed));
    assert!(t.store().get(&e).is_some(), "悬挂条目——既有覆盖写契约，非谓词引入");
    let hit = t.scan::<ByOpenAssignee>(&7u64.to_be_bytes());
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].1.as_ref().unwrap().status, 1, "扫描返回了谓词拒绝的行");

    // 纪律：delete 用「当时存储的行」（= 生成旧条目那一行）再 put 新状态。
    // 顺序颠倒的补救见下。
    let mut t2 = <Ticket as Document>::collection(TestStore::slatedb_mem());
    t2.put(&k, &open);
    t2.delete(&k, &open); // 删除集合由旧状态派生 → 条目清掉
    t2.put(&k, &closed);
    assert!(t2.store().get(&e).is_none());
    assert!(t2.scan::<ByOpenAssignee>(&7u64.to_be_bytes()).is_empty());

    // delete 的删除集合也由传入行派生：closed 行不产条目，所以
    // delete(closed) 清不掉悬挂条目，必须用旧状态的行再删一次
    t.delete(&k, &closed);
    assert!(t.store().get(&e).is_some(), "delete 沿 entry_pairs 派生，closed 行是 no-op");
    t.delete(&k, &open);
    assert!(t.store().get(&e).is_none());
    assert!(t.scan::<ByOpenAssignee>(&7u64.to_be_bytes()).is_empty());
}

#[test]
fn function_index_empty_vec_skips_row() {
    let mut t = <Ticket as Document>::collection(TestStore::slatedb_mem());
    let open = mk_ticket(1, 7, 10, 3, 0, "rust,kv");
    let closed = mk_ticket(2, 7, 20, 5, 1, "rust");
    t.put(&open.0, &open.1);
    t.put(&closed.0, &closed.1);

    // 被接纳的行 fan out 成 2 条 token entry；空 Vec → 0 条
    assert_eq!(
        ByLiveToken::entry_pairs(<Ticket as Document>::NS_PREFIX, &open.0, &open.1).len(),
        2
    );
    assert!(ByLiveToken::entry_pairs(<Ticket as Document>::NS_PREFIX, &closed.0, &closed.1).is_empty());
    assert!(t.store().get(&t.index_key::<ByLiveToken>(&closed.0, &closed.1)).is_none());

    // 扫描只见被接纳的行
    let rust = t.scan::<ByLiveToken>(b"rust");
    assert_eq!(rust.len(), 1);
    assert_eq!(rust[0].0.decoded.id, 1);

    // delete 对称：写入集合与删除集合同源（空 Vec 的行删除是 no-op）
    t.delete(&open.0, &open.1);
    assert_eq!(t.scan::<ByLiveToken>(b"rust").len(), 0);
}

#[test]
fn function_index_with_where_filters_row() {
    // 函数索引形态叠加声明谓词：切分无条件，过滤在 where——同一个谓词对
    // 两种索引形态一视同仁（都是 admits 覆盖 + entry_pairs 短路）。
    let mut t = <Ticket as Document>::collection(TestStore::slatedb_mem());
    let open = mk_ticket(1, 7, 10, 3, 0, "rust,kv");
    let closed = mk_ticket(2, 7, 20, 5, 1, "rust");
    t.put(&open.0, &open.1);
    t.put(&closed.0, &closed.1);

    assert_eq!(
        ByOpenToken::entry_pairs(<Ticket as Document>::NS_PREFIX, &open.0, &open.1).len(),
        2
    );
    assert!(ByOpenToken::entry_pairs(<Ticket as Document>::NS_PREFIX, &closed.0, &closed.1).is_empty());
    assert!(!ByOpenToken::admits(&closed.1));

    // 与空 Vec 写法等价（by_live_token 的 live_tokens 内嵌同一条件）
    assert_eq!(
        t.scan::<ByOpenToken>(b"rust").len(),
        t.scan::<ByLiveToken>(b"rust").len()
    );
    assert_eq!(t.scan::<ByOpenToken>(b"rust").len(), 1);
}

