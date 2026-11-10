#![allow(clippy::unwrap_used, clippy::expect_used)] // a test states its expectations directly
//! 验证「SQL 的多对多就是一张普通的表」在 okm 里成立：关系表 = 键为两个身份的文档集合，
//! 反向方向 = 声明索引。若这里四个断言成立，`Junction` 那套机械没有独有能力。
//!
//! 断言的是机制，不是实现细节：
//! 1. 行上带关系属性（`since_ms`），不是只有两个 id；
//! 2. 反向按 member 扫索引，**回表**拿回属性（junction 的反向面只能给回身份）；
//! 3. `purge` 一行，反向索引条目跟着消失（同一个 batch，靠 `put`/`purge` 而不是靠人记）；
//! 4. `rebuild_faces` 从行重建索引面后，结果不变（派生状态可重建）。

use okm_async::TestStore;
use okm_core::{DocumentEncode, KeyEncode};

/// 复合主键：一张普通的表，两列就是关系表的两端。
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct MembershipKey {
    pub channel: u64,
    pub member: u64,
}

/// 关系行。`member` 是列也是索引字段——SQL 里索引建在列上，这里一样。
#[derive(DocumentEncode, Clone)]
#[ok_ns(200)]
#[ok_ref(MembershipKey)]
#[ok_index(by_member { fields(member) })]
pub struct Membership {
    pub member: u64,
    pub since_ms: i64,
    pub role: u8,
}

#[test]
fn a_relation_table_answers_both_directions_with_its_payload() {
    let store = TestStore::slatedb_mem();
    let table: okm_core::Collection<TestStore, MembershipKey, Membership> =
        okm_core::Collection::new(store.clone());

    let k = |channel: u64, member: u64| MembershipKey { channel, member };
    let row = |member: u64, since_ms: i64| Membership {
        member,
        since_ms,
        role: 1,
    };

    table.put(&k(7, 101), &row(101, 1_700_000_000_000)).unwrap();
    table.put(&k(7, 102), &row(102, 1_700_000_001_000)).unwrap();
    table.put(&k(9, 101), &row(101, 1_700_000_002_000)).unwrap();

    // ---- 反向：按 member 扫索引并回表 ----
    let probe = 101u64.to_be_bytes();
    let hits = table
        .scan::<__OkmIndex_Membership_by_member>(&probe)
        .unwrap();
    assert_eq!(hits.len(), 2, "member 101 is in two channels");
    let mut sines: Vec<i64> = hits
        .iter()
        .map(|(_, doc)| doc.as_ref().expect("fetch-back returns the row").since_ms)
        .collect();
    sines.sort();
    assert_eq!(
        sines,
        vec![1_700_000_000_000, 1_700_000_002_000],
        "the reverse read carries the relation's payload, not just the ids"
    );

    // ---- purge 一行：索引条目跟着走 ----
    table.purge(&k(9, 101)).unwrap();
    let hits = table
        .scan::<__OkmIndex_Membership_by_member>(&probe)
        .unwrap();
    assert_eq!(hits.len(), 1, "the index entry left with the row");

    // ---- 派生面可重建：清空后从行重建，结果不变 ----
    table.rebuild_faces(64).unwrap();
    let hits = table
        .scan::<__OkmIndex_Membership_by_member>(&probe)
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the index is derived state, and it comes back"
    );

    // ---- 正向：channel 是复合主键的第一列，前缀扫一次拿到这个频道的全部成员 ----
    let probe = okm_core::PrefixProbe::new(7u64.to_be_bytes().to_vec());
    let keys: Vec<MembershipKey> = table
        .scan_keys(&probe)
        .map(|k| k.expect("a decodable key"))
        .collect();
    assert_eq!(
        keys,
        vec![k(7, 101), k(7, 102)],
        "the first key column is the forward direction, exactly as in SQL"
    );
}
