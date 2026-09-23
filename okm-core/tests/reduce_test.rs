//! 跨行预聚合集成测试：`#[ok_reduce(Logic { group(...) })]` 声明 →
//! derive 生成 `Reduce` impl + Document hook；Collection::put/delete 读改写。
//! 覆盖：计数+求和复合 acc 的可逆往返、delete_by_pkey 同一路径、
//! 同 group 多次 fold 累积、scan_reduces 全组扫描、entry 布局
//! `[ns 2B][slot 1B][group 段]`（slot 续接索引计数器）。

use okm_core::{Reduce, ReduceLogic, ReduceCodec, TestStore, Document, DocumentEncode};

/// PostKey：代理主键。
#[derive(okm_core::KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PostKey {
    pub id: u64,
}

/// Post 行：按 author 分组做 count + title_len 求和。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]
#[ok_reduce(AuthorStats { group(author_id) })]
#[ok_ns(21)]
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

impl ReduceCodec for CountSum {
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

/// 用户侧逻辑 half：okm-core 不感知语义，只负责喂行与存取 acc。
pub struct AuthorStats;

impl ReduceLogic for AuthorStats {
    type Document = Post;
    type Acc = CountSum;
    fn fold(acc: &mut CountSum, _key: &PostKey, item: &Post) {
        acc.count += 1;
        acc.sum += item.title_len as u64;
    }
    fn unfold(acc: &mut CountSum, _key: &PostKey, item: &Post) {
        acc.count -= 1;
        acc.sum -= item.title_len as u64;
    }
}

#[test]
fn fold_unfold_roundtrip_is_exact() {
    let mut t = <Post as Document>::collection(TestStore::slatedb_mem());

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
    let acc = okm_core::reduce_get::<_, AuthorStats>(t.store(), <Post as Document>::NS_PREFIX, &k1, &r1).expect("group exists");
    assert_eq!(acc, CountSum { count: 2, sum: 42 });

    // delete_by_pkey 走同一条 unfold 路径（内部 get 出 document）。
    t.delete_by_pkey(&k2);
    let acc = okm_core::reduce_get::<_, AuthorStats>(t.store(), <Post as Document>::NS_PREFIX, &k1, &r1).expect("group exists");
    assert_eq!(acc, CountSum { count: 1, sum: 30 });

    // 可逆往返：删空后 acc 回到单位元。
    t.delete_by_pkey(&k1);
    let acc = okm_core::reduce_get::<_, AuthorStats>(t.store(), <Post as Document>::NS_PREFIX, &k1, &r1).expect("entry survives");
    assert_eq!(acc, CountSum::default());

    // acc 独立生命周期：组空了 entry 仍在（okm-core 不做零值 GC）。
    let all = okm_core::scan_reduces::<_, AuthorStats>(t.store(), <Post as Document>::NS_PREFIX);
    assert_eq!(all.len(), 2);
}

/// TopicKey：多字段 key —— group 直接命名 key 字段（ADR-0024）。
#[derive(okm_core::KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct TopicKey {
    pub forum: u32,
    pub id: u64,
}

/// Topic 行：GROUP 命名 key 字段 `forum`，fold 聚合 key 字段 `id`
/// 的最大值（MaxInstanceId 形态，ADR-0024 §Consequences）。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(TopicKey)]
#[ok_reduce(TopicStats { group(forum) })]
#[ok_ns(22)]
pub struct Topic {
    pub title_len: u32,
}

/// acc = 已见最大 key.id（0 = 空，u64 非负域内可逆 unfold：
/// 只在删除的恰是当前最大值时回退是 Max 语义做不到的——此处 unfold
/// 仅对非最大值行调用即可保持精确；测试只走 put/单值路径）。
#[derive(Default, Clone, Debug, PartialEq)]
pub struct MaxId(pub u64);

impl ReduceCodec for MaxId {
    fn encode_acc(&self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }
    fn decode_acc(bytes: &[u8]) -> Self {
        MaxId(u64::from_be_bytes(bytes.try_into().unwrap()))
    }
}

pub struct TopicStats;

impl ReduceLogic for TopicStats {
    type Document = Topic;
    type Acc = MaxId;
    fn fold(acc: &mut MaxId, key: &TopicKey, _item: &Topic) {
        acc.0 = acc.0.max(key.id);
    }
    fn unfold(_acc: &mut MaxId, _key: &TopicKey, _item: &Topic) {
        // Max 不可逆；本测试不删除，保持 unfold 空实现。
    }
}

#[test]
fn group_and_fold_use_key_fields() {
    let mut t = <Topic as Document>::collection(TestStore::slatedb_mem());

    let k1 = TopicKey { forum: 7, id: 100 };
    let k2 = TopicKey { forum: 7, id: 42 };
    let k3 = TopicKey { forum: 8, id: 5 };
    let r = Topic { title_len: 3 };
    t.put(&k1, &r);
    t.put(&k2, &r);
    t.put(&k3, &r);

    // group 段 = key 字段 forum 的编码（u32 4B BE），而非任何 payload。
    let ek = <TopicStats as Reduce>::entry_key(<Topic as Document>::NS_PREFIX, &k1, &r);
    assert_eq!(ek.len(), 4 + 4);
    assert_eq!(&ek[4..], &7u32.to_be_bytes());

    // fold 聚合 key 字段 id：forum 7 → max(100, 42) = 100。
    let acc = okm_core::reduce_get::<_, TopicStats>(t.store(), <Topic as Document>::NS_PREFIX, &k1, &r)
        .expect("group exists");
    assert_eq!(acc, MaxId(100));

    // forum 8 独立成组。
    let acc8 =
        okm_core::reduce_get::<_, TopicStats>(t.store(), <Topic as Document>::NS_PREFIX, &k3, &r)
            .expect("group exists");
    assert_eq!(acc8, MaxId(5));
}

#[test]
fn entry_layout_is_ns_slot_group() {
    let mut t = <Post as Document>::collection(TestStore::slatedb_mem());
    let k = PostKey { id: 9 };
    let r = Post {
        author_id: 55,
        title_len: 1,
    };
    t.put(&k, &r);

    // author_id 是 u64 → group 段 = 8B BE；reduce 走独立段 0x2
    // （计数 1 → slot 0x2001，ADR-0016：不再续接索引计数器）。
    let ek = <AuthorStats as Reduce>::entry_key(<Post as Document>::NS_PREFIX, &k, &r);
    assert_eq!(ek.len(), 4 + 8);
    assert_eq!(&ek[..2], &[0, 21]);
    assert_eq!(&ek[2..4], &[0x20, 0x01]);
    assert_eq!(&ek[4..], &55u64.to_be_bytes());
}
