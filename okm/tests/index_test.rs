//! 二级索引集成测试（ADR-0006 Row 形状）：RowEncode 派生、Table 装配点
//! put/scan 回表、entry 布局 hex 锁定（slot 从 1 起，0 保留给主表）、
//! 最左前缀扫描、includes 覆盖。

use okm::{KeyEncode, KvEngine, KvIndex, MockStore, Row, RowEncode, Table};

// marker struct 生成在 derive 展开点（本文件），直接引用
use __OkmIndex_User_by_name as ByName;
use __OkmIndex_User_by_org_name as ByOrgName;

/// UserKey：org 内的用户身份（主键）。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(9)]
pub struct UserKey {
    pub org_id: u32,
    pub name: [u8; 8],
    pub user_id: u64,
}

/// User 行：kv_ref 引用主键 + 属性字段 + 索引声明。
/// 索引 fields+includes 必须落在 key 声明序前缀臂上（索引排序字节
/// 全部来自 key 结构体，行载荷字段不参与索引排序——覆盖字段
/// includes 也必须是 key 前缀的延续，如 by_org_name 的 includes(name)）。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(UserKey)]
#[kv_index(
    by_name { fields(org_id, name) },
    by_org_name { fields(org_id), includes(name) },
)]
pub struct User {
    pub reputation: u32,
    pub bio_len: u16,
}

fn mk(org: u32, name: &[u8; 8], uid: u64) -> (UserKey, User) {
    let mut nm = [0u8; 8];
    nm[..name.len()].copy_from_slice(name);
    (
        UserKey {
            org_id: org,
            name: nm,
            user_id: uid,
        },
        User {
            reputation: 100,
            bio_len: 2,
        },
    )
}

#[test]
fn table_put_writes_primary_and_indexes() {
    let mut t: Table<MockStore, UserKey, User> = Table::new(MockStore::default(), 9);
    let (k, r) = mk(7, b"alice\0\0\0", 101);
    t.put(&k, &r);

    // 主键写入 slot 0：头 [0,9,0] + key payload
    let kl = <UserKey as KeyEncode>::KEY_LEN;
    let pk = t.primary_key(&k);
    assert_eq!(&pk[..3], &[0, 9, 0]);
    assert_eq!(&pk[3..], &k.encode()[..]);
    // value = 行载荷 TLV，可解码回原行
    let raw = t.store().get(&pk).unwrap();
    let dec = <User as Row>::decode_payload(&raw);
    assert_eq!(dec, r);

    // 索引 entry：slot 1（by_name）、slot 2（by_org_name）
    // entry value 恒为空，断言对象是 entry key 本身
    let e1 = t.index_key::<ByName>(&k);
    assert_eq!(&e1[..3], &[0, 9, 1]);
    assert_eq!(&e1[e1.len() - kl..], &k.encode()[..]);
    assert!(t.store().get(&e1).is_some());
    let e2 = t.index_key::<ByOrgName>(&k);
    assert_eq!(&e2[..3], &[0, 9, 2]);
    assert!(t.store().get(&e2).is_some());

    // delete：主键 + 全部声明索引一并移除（声明即注册表）
    t.delete(&k);
    assert!(t.store().get(&t.primary_key(&k)).is_none());
    assert!(t.store().get(&t.index_key::<ByName>(&k)).is_none());
    assert!(t.store().get(&t.index_key::<ByOrgName>(&k)).is_none());
}

#[test]
fn scan_via_index_returns_rows() {
    let mut t: Table<MockStore, UserKey, User> = Table::new(MockStore::default(), 9);
    let (k1, r1) = mk(7, b"alice\0\0\0", 101);
    let (k2, r2) = mk(7, b"alice\0\0\0", 102);
    let (k3, r3) = mk(7, b"bob\0\0\0\0\0", 103);
    let (k4, r4) = mk(8, b"alice\0\0\0", 104);
    for (k, r) in [(&k1, &r1), (&k2, &r2), (&k3, &r3), (&k4, &r4)] {
        t.put(k, r);
    }

    // 最左前缀 org=7 → 3 行回表
    let rows = t.scan::<ByName>(&7u32.to_be_bytes());
    assert_eq!(rows.len(), 3);
    // (org=7, name=alice) → 2 行，按主键字节序
    let mut p = Vec::new();
    p.extend_from_slice(&7u32.to_be_bytes());
    p.extend_from_slice(b"alice\0\0\0");
    let rows = t.scan::<ByName>(&p);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, k1);
    assert_eq!(rows[1].0, k2);
    // 回表：payload 完整可读
    assert_eq!(rows[0].1.as_ref().unwrap(), &r1);
    assert_eq!(rows[1].1.as_ref().unwrap(), &r2);

    // includes 索引（fields(org_id) includes(name)）同样最左前缀扫
    assert_eq!(t.scan::<ByOrgName>(&7u32.to_be_bytes()).len(), 3);
    // 空前缀 = 全索引扫描
    assert_eq!(t.scan::<ByName>(&[]).len(), 4);
}

#[test]
fn entry_layout_hex_lock() {
    // 直接锁定 by_name entry 字节布局（slot 1 起）
    let (k, _r) = mk(7, b"alice\0\0\0", 101);
    let e = ByName::encode_entry(9, &k);
    let kl = <UserKey as KeyEncode>::KEY_LEN; // 4+8+8=20
    assert_eq!(kl, 20);
    assert_eq!(&e[..3], &[0, 9, 1]); // [ns=9 2B][slot=1]
    assert_eq!(&e[3..7], &7u32.to_be_bytes());
    assert_eq!(&e[7..15], b"alice\0\0\0");
    // 尾部 = 完整主键 ID
    assert_eq!(&e[e.len() - kl..], &k.encode()[..]);
    assert_eq!(e.len(), 3 + 4 + 8 + kl);

    // by_org_name：slot 2，carried = org_id(4) + name(8) 覆盖
    let e2 = ByOrgName::encode_entry(9, &k);
    assert_eq!(&e2[..3], &[0, 9, 2]);
    assert_eq!(&e2[3..7], &7u32.to_be_bytes());
    assert_eq!(&e2[7..15], b"alice\0\0\0"); // includes(name) 紧随 fields
    assert_eq!(&e2[15..], &k.encode()[..]);
}

#[test]
fn slot_allocation() {
    // slot 从 1 起：0 保留给主表，主键首字段与索引头不再歧义
    assert_eq!(<ByName as KvIndex>::SLOT, 1);
    assert_eq!(<ByOrgName as KvIndex>::SLOT, 2);
    assert_eq!(<ByName as KvIndex>::FIELDS, &["org_id", "name"]);
    assert_eq!(<ByOrgName as KvIndex>::INCLUDES, &["name"]);
    assert_eq!(okm::PRIMARY_SLOT, 0);
}

#[test]
fn row_value_tlv_roundtrip() {
    let (_k, r) = mk(7, b"alice\0\0\0", 101);
    let enc = r.encode_payload();
    let dec = <User as Row>::decode_payload(&enc);
    assert_eq!(dec, r);
}
