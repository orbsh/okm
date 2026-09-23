//! Preset reduce combinators (ADR-0023) integration tests: Count / Sum /
//! HighWater / LowWater as one-line declarations, in BOTH the grouped
//! (`group(f)`) and the no-group (whole-table single group) modes.
//! Semantics coverage: fold/unfold reversibility (Count/Sum exact),
//! watermark no-op unfold (HighWater/LowWater), u64 BE acc layout, key-field
//! aggregation (ADR-0024 two-source rule).
//!
//! The derive expands each preset to a local forward marker
//! (`__OkmReduce_<Doc>_<n>`); tests name those types via
//! `okm_core::reduce::scan_reduces` / `Reduce::entry_key` generics.

use okm_core::{Document, DocumentEncode, KeyEncode, Quant, Reduce, ReduceCodec, ReduceLogic, TestStore};

/// 分组模式：按 forum 分组，四种 preset 各占一个 slot。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PostKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]
#[ok_ns(23)]
#[ok_reduce(Count { group(forum) })]
#[ok_reduce(Sum(views) { group(forum) })]
#[ok_reduce(HighWater(views) { group(forum) })]
#[ok_reduce(LowWater(views) { group(forum) })]
pub struct Post {
    pub forum: u64,
    pub views: u32,
}

/// 不分组模式：无 group 块 = 全表单一组。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct VoteKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(VoteKey)]
#[ok_ns(24)]
#[ok_reduce(Count)]
#[ok_reduce(HighWater(score))]
pub struct Vote {
    pub score: u32,
}

#[test]
fn grouped_presets_fold_unfold_scan() {
    let mut t = <Post as Document>::collection(TestStore::slatedb_mem());
    let r1 = Post { forum: 7, views: 100 };
    let r2 = Post { forum: 7, views: 40 };
    let r3 = Post { forum: 8, views: 9 };
    let k = |id: u64| PostKey { id };
    t.put(&k(1), &r1);
    t.put(&k(2), &r2);
    t.put(&k(3), &r3);

    let ns = <Post as Document>::NS_PREFIX;
    let probe = |store: &TestStore, forum: u64| -> (Option<u64>, Option<u64>, Option<u64>, Option<okm_core::LowAcc>) {
        let gb = forum.to_be_bytes().to_vec();
        let find = |v: Vec<(Vec<u8>, u64)>| {
            v.into_iter()
                .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
                .map(|(_, a)| a)
        };
        let find_min = |v: Vec<(Vec<u8>, okm_core::LowAcc)>| {
            v.into_iter()
                .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
                .map(|(_, a)| a)
        };
        (
            find(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_0>(store, ns)),
            find(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_1>(store, ns)),
            find(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_2>(store, ns)),
            find_min(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_3>(store, ns)),
        )
    };

    // forum 7：count=2, sum=140, max=100, min=40。
    let (c, s, mx, mn) = probe(t.store(), 7);
    assert_eq!(c, Some(2));
    assert_eq!(s, Some(140));
    assert_eq!(mx, Some(100));
    assert_eq!(mn, Some(okm_core::LowAcc(40)));
    // forum 8：count=1, sum=9。
    let (c, s, mx, mn) = probe(t.store(), 8);
    assert_eq!(c, Some(1));
    assert_eq!(s, Some(9));
    assert_eq!(mx, Some(9));
    assert_eq!(mn, Some(okm_core::LowAcc(9)));

    // Count/Sum unfold 精确：删 r2 后 sum 回到 100；HighWater/LowWater
    // no-op unfold：max 仍 100（watermark），min 仍 40（低水位不动）。
    t.delete_by_pkey(&k(2));
    let (c, s, mx, mn) = probe(t.store(), 7);
    assert_eq!(c, Some(1));
    assert_eq!(s, Some(100));
    assert_eq!(mx, Some(100));
    assert_eq!(mn, Some(okm_core::LowAcc(40)));
}

#[test]
fn no_group_mode_single_entry() {
    let mut t = <Vote as Document>::collection(TestStore::slatedb_mem());
    let r = Vote { score: 5 };
    t.put(&VoteKey { id: 1 }, &r);
    t.put(&VoteKey { id: 2 }, &Vote { score: 12 });

    let ns = <Vote as Document>::NS_PREFIX;
    // 不分组：entry key = [ns 2B][slot 2B]，无 group 段。每组恰一条。
    let count = okm_core::reduce::scan_reduces::<_, __OkmReduce_Vote_0>(t.store(), ns);
    assert_eq!(count.len(), 1);
    assert_eq!(count[0].0.len(), 0, "no group segment");
    assert_eq!(count[0].1, 2);

    let max = okm_core::reduce::scan_reduces::<_, __OkmReduce_Vote_1>(t.store(), ns);
    assert_eq!(max.len(), 1);
    assert_eq!(max[0].1, 12);

    // slot 续接声明序：Count=0x2001，HighWater=0x2002（独立 reduce 段计数器）。
    let ek = <__OkmReduce_Vote_0 as Reduce>::entry_key(ns, &VoteKey { id: 1 }, &r);
    assert_eq!(ek.len(), 4);
    assert_eq!(&ek[2..4], &[0x20, 0x01]);
    let ek = <__OkmReduce_Vote_1 as Reduce>::entry_key(ns, &VoteKey { id: 1 }, &r);
    assert_eq!(&ek[2..4], &[0x20, 0x02]);
}

#[test]
fn preset_aggregates_key_field() {
    // ADR-0024 × 0023 交点：preset 的聚合字段可以是 key 字段。
    #[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
    pub struct TaskKey {
        pub list: u32,
        pub seq: u64,
    }

    #[derive(DocumentEncode, Clone, PartialEq, Debug)]
    #[ok_ref(TaskKey)]
    #[ok_ns(25)]
    #[ok_reduce(HighWater(seq) { group(list) })]
    pub struct Task {
        pub title_len: u32,
    }

    // 直接驱动 ReduceLogic 泛型路径：seq 是 key 字段，fold 聚合它。
    let mut acc = <__OkmReduce_Task_0 as ReduceLogic>::Acc::default();
    let k1 = TaskKey { list: 3, seq: 90 };
    let k2 = TaskKey { list: 3, seq: 41 };
    let r = Task { title_len: 0 };
    <__OkmReduce_Task_0 as ReduceLogic>::fold(&mut acc, &k1, &r);
    <__OkmReduce_Task_0 as ReduceLogic>::fold(&mut acc, &k2, &r);
    assert_eq!(acc, 90);
    // unfold no-op：watermark 不落。
    <__OkmReduce_Task_0 as ReduceLogic>::unfold(&mut acc, &k1, &r);
    assert_eq!(acc, 90);
    // acc 的 u64 BE wire 形状（bindings 的字节契约）。
    let bytes = <u64 as ReduceCodec>::encode_acc(&acc);
    assert_eq!(bytes, 90u64.to_be_bytes());
}

/// Sum 的 acc 类型跟字段走（ADR-0023 修订）：signed 字段 i64 累加
/// （含负数精确可逆），Quant<f64,P> 字段定点 wire 域累加。
#[derive(okm_core::KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct LedgerKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(LedgerKey)]
#[ok_ns(26)]
#[ok_reduce(Sum(delta) { group(account) })]
#[ok_reduce(Sum(amount) { group(account) })]
pub struct Entry {
    pub account: u64,
    pub delta: i64,
    pub amount: Quant<2>,
}

#[test]
fn sum_acc_type_follows_field() {
    let mut t = <Entry as Document>::collection(TestStore::slatedb_mem());
    let k = |id: u64| LedgerKey { id };
    let r_pos = Entry { account: 7, delta: 100, amount: Quant::<2>::new(12.5) };
    let r_neg = Entry { account: 7, delta: -30, amount: Quant::<2>::new(-2.0) };
    t.put(&k(1), &r_pos);
    t.put(&k(2), &r_neg);

    let ns = <Entry as Document>::NS_PREFIX;
    let gb = 7u64.to_be_bytes().to_vec();
    // i64 acc：100 + (−30) = 70，负数参与求和精确。
    let sum_i = okm_core::reduce::scan_reduces::<_, __OkmReduce_Entry_0>(t.store(), ns)
        .into_iter()
        .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
        .map(|(_, a)| a);
    assert_eq!(sum_i, Some(70i64));
    // Quant<2> 定点 wire 累加：1250 + (−200) = 1050（= 10.50）。
    let sum_q = okm_core::reduce::scan_reduces::<_, __OkmReduce_Entry_1>(t.store(), ns)
        .into_iter()
        .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
        .map(|(_, a)| a);
    assert_eq!(sum_q, Some(1050i64));

    // 可逆：删 r_neg 后两者回到 100 / 1250。
    t.delete_by_pkey(&k(2));
    let sum_i = okm_core::reduce::scan_reduces::<_, __OkmReduce_Entry_0>(t.store(), ns)
        .into_iter()
        .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
        .map(|(_, a)| a);
    let sum_q = okm_core::reduce::scan_reduces::<_, __OkmReduce_Entry_1>(t.store(), ns)
        .into_iter()
        .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
        .map(|(_, a)| a);
    assert_eq!(sum_i, Some(100i64));
    assert_eq!(sum_q, Some(1250i64));
}
