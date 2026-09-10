//! 跨行预聚合集成测试：`#[kv_aggregate(Logic { group(...) })]` 声明 →
//! derive 生成 `Aggregate` impl + Row hook；Table::put/delete 读改写。
//! 覆盖：计数+求和复合 acc 的可逆往返、delete_by_pkey 同一路径、
//! 同 group 多次 fold 累积、scan_aggregates 全组扫描、entry 布局
//! `[ns 2B][slot 1B][group 段]`（slot 续接索引计数器）。

use okm::{Aggregate, AggregateLogic, AggCodec, MockStore, Row, RowEncode};

/// PostKey：代理主键。
#[derive(okm::KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(21)]
pub struct PostKey {
    pub id: u64,
}

/// Post 行：按 author 分组做 count + title_len 求和。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(PostKey)]
#[kv_aggregate(AuthorStats { group(author_id) })]
pub struct Post {
    pub author_id: u64,
    pub title_len: u32,
}

/// 复合 acc：count + sum（均值 = sum/count 的正确底座，可逆）。
#[derive(Default, Clone, Debug, PartialEq)]
pub struct CountSum {
    pub count: u64,
    pub sum: u64,
}

impl AggCodec for CountSum {
    fn encode_acc(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(16);
        b.extend_from_slice(&self.count.to_be_bytes());
        b.extend_from_slice(&self.sum.to_be_bytes());
        b
    }
    fn decode_acc(bytes: &[u8]) -> Self {
        assert!(bytes.len() == 16, "CountSum acc must be 16 bytes BE");
        CountSum {
            count: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
            sum: u64::from_be_bytes(bytes[8..].try_into().unwrap()),
        }
    }
}

/// 用户侧逻辑 half：okm 不感知语义，只负责喂行与存取 acc。
pub struct AuthorStats;

impl AggregateLogic for AuthorStats {
    type Row = Post;
    type Acc = CountSum;
    fn fold(acc: &mut CountSum, item: &Post) {
        acc.count += 1;
        acc.sum += item.title_len as u64;
    }
    fn unfold(acc: &mut CountSum, item: &Post) {
        acc.count -= 1;
        acc.sum -= item.title_len as u64;
    }
}

#[test]
fn fold_unfold_roundtrip_is_exact() {
    let mut t = <Post as Row>::table(MockStore::default(), 21);

    let k1 = PostKey { id: 1 };
    let r1 = Post {
        author_id: 100,
        title_len: 30,
    };
    let k2 = PostKey { id: 2 };
    let r2 = Post {
        author_id: 100,
        title_len: 12,
    };
    let k3 = PostKey { id: 3 };
    let r3 = Post {
        author_id: 200,
        title_len: 7,
    };
    t.put(&k1, &r1);
    t.put(&k2, &r2);
    t.put(&k3, &r3);

    // author 100: count=2, sum=42；author 200: count=1, sum=7。
    let acc = okm::aggregate_get::<_, AuthorStats>(t.store(), 21, &k1, &r1).expect("group exists");
    assert_eq!(acc, CountSum { count: 2, sum: 42 });

    // delete_by_pkey 走同一条 unfold 路径（内部 get 出 row）。
    t.delete_by_pkey(&k2);
    let acc = okm::aggregate_get::<_, AuthorStats>(t.store(), 21, &k1, &r1).expect("group exists");
    assert_eq!(acc, CountSum { count: 1, sum: 30 });

    // 可逆往返：删空后 acc 回到单位元。
    t.delete_by_pkey(&k1);
    let acc = okm::aggregate_get::<_, AuthorStats>(t.store(), 21, &k1, &r1).expect("entry survives");
    assert_eq!(acc, CountSum::default());

    // acc 独立生命周期：组空了 entry 仍在（okm 不做零值 GC）。
    let all = okm::scan_aggregates::<_, AuthorStats>(t.store(), 21);
    assert_eq!(all.len(), 2);
}

#[test]
fn entry_layout_is_ns_slot_group() {
    let mut t = <Post as Row>::table(MockStore::default(), 21);
    let k = PostKey { id: 9 };
    let r = Post {
        author_id: 55,
        title_len: 1,
    };
    t.put(&k, &r);

    // author_id 是 u64 → group 段 = 8B BE；slot 续接索引计数器（无索引 → 1）。
    let ek = <AuthorStats as Aggregate>::entry_key(21, &k, &r);
    assert_eq!(ek.len(), 3 + 8);
    assert_eq!(&ek[..2], &21u16.to_be_bytes());
    assert_eq!(ek[2], 1);
    assert_eq!(&ek[3..], &55u64.to_be_bytes());
}
