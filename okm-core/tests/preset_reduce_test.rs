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

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::{
    Document, DocumentEncode, KeyEncode, Quant, Reduce, ReduceCodec, ReduceLogic, TestStore,
};

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
    let t = <Post as Document>::collection(TestStore::slatedb_mem());
    let r1 = Post {
        forum: 7,
        views: 100,
    };
    let r2 = Post {
        forum: 7,
        views: 40,
    };
    let r3 = Post { forum: 8, views: 9 };
    let k = |id: u64| PostKey { id };
    t.put(&k(1), &r1).unwrap();
    t.put(&k(2), &r2).unwrap();
    t.put(&k(3), &r3).unwrap();

    let ns = <Post as Document>::NS_PREFIX;
    let probe = |store: &TestStore,
                 forum: u64|
     -> (
        Option<u64>,
        Option<u64>,
        Option<u64>,
        Option<okm_core::LowAcc>,
    ) {
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
            find(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_0>(store, ns).unwrap()),
            find(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_1>(store, ns).unwrap()),
            find(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_2>(store, ns).unwrap()),
            find_min(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_3>(store, ns).unwrap()),
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
    t.purge(&k(2)).unwrap();
    let (c, s, mx, mn) = probe(t.store(), 7);
    assert_eq!(c, Some(1));
    assert_eq!(s, Some(100));
    assert_eq!(mx, Some(100));
    assert_eq!(mn, Some(okm_core::LowAcc(40)));
}

#[test]
fn no_group_mode_single_entry() {
    let t = <Vote as Document>::collection(TestStore::slatedb_mem());
    let r = Vote { score: 5 };
    t.put(&VoteKey { id: 1 }, &r).unwrap();
    t.put(&VoteKey { id: 2 }, &Vote { score: 12 }).unwrap();

    let ns = <Vote as Document>::NS_PREFIX;
    // 不分组：entry key = [ns 2B][slot 2B]，无 group 段。每组恰一条。
    let count = okm_core::reduce::scan_reduces::<_, __OkmReduce_Vote_0>(t.store(), ns).unwrap();
    assert_eq!(count.len(), 1);
    assert_eq!(count[0].0.len(), 0, "no group segment");
    assert_eq!(count[0].1, 2);

    let max = okm_core::reduce::scan_reduces::<_, __OkmReduce_Vote_1>(t.store(), ns).unwrap();
    assert_eq!(max.len(), 1);
    assert_eq!(max[0].1, 12);

    // slot 续接声明序：Count=0x4001，HighWater=0x4002（独立 reduce 段计数器；族 0x5）。
    let ek = <__OkmReduce_Vote_0 as Reduce>::entry_key(ns, &VoteKey { id: 1 }, &r);
    assert_eq!(ek.len(), 4);
    assert_eq!(&ek[2..4], &[0x40, 0x01]);
    let ek = <__OkmReduce_Vote_1 as Reduce>::entry_key(ns, &VoteKey { id: 1 }, &r);
    assert_eq!(&ek[2..4], &[0x40, 0x02]);
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
    let t = <Entry as Document>::collection(TestStore::slatedb_mem());
    let k = |id: u64| LedgerKey { id };
    let r_pos = Entry {
        account: 7,
        delta: 100,
        amount: Quant::<2>::new(12.5),
    };
    let r_neg = Entry {
        account: 7,
        delta: -30,
        amount: Quant::<2>::new(-2.0),
    };
    t.put(&k(1), &r_pos).unwrap();
    t.put(&k(2), &r_neg).unwrap();

    let ns = <Entry as Document>::NS_PREFIX;
    let gb = 7u64.to_be_bytes().to_vec();
    // i64 acc：100 + (−30) = 70，负数参与求和精确。
    let sum_i = okm_core::reduce::scan_reduces::<_, __OkmReduce_Entry_0>(t.store(), ns)
        .unwrap()
        .into_iter()
        .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
        .map(|(_, a)| a);
    assert_eq!(sum_i, Some(70i64));
    // Quant<2> 定点 wire 累加：1250 + (−200) = 1050（= 10.50）。
    let sum_q = okm_core::reduce::scan_reduces::<_, __OkmReduce_Entry_1>(t.store(), ns)
        .unwrap()
        .into_iter()
        .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
        .map(|(_, a)| a);
    assert_eq!(sum_q, Some(1050i64));

    // 可逆：删 r_neg 后两者回到 100 / 1250。
    t.purge(&k(2)).unwrap();
    let sum_i = okm_core::reduce::scan_reduces::<_, __OkmReduce_Entry_0>(t.store(), ns)
        .unwrap()
        .into_iter()
        .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
        .map(|(_, a)| a);
    let sum_q = okm_core::reduce::scan_reduces::<_, __OkmReduce_Entry_1>(t.store(), ns)
        .unwrap()
        .into_iter()
        .find(|(sfx, _)| sfx.as_slice() == gb.as_slice())
        .map(|(_, a)| a);
    assert_eq!(sum_i, Some(100i64));
    assert_eq!(sum_q, Some(1250i64));
}

/// ADR-0034: reduces travel as their own derived table — `(table, reduce,
/// group, value)` — because a restored table whose watermark is missing hands
/// out ids it already used.
#[cfg(feature = "parquet")]
#[test]
fn the_reduce_table_exports_as_its_own_file() {
    use arrow::array::{Array, BinaryArray, UInt16Array};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let dir = tempfile::tempdir().unwrap();
    let t: okm_core::Collection<TestStore, PostKey, Post> =
        okm_core::Collection::new(TestStore::slatedb_mem());
    for (forum, views) in [(1u64, 10u32), (1, 40), (2, 7)] {
        t.put(
            &PostKey {
                id: forum * 100 + views as u64,
            },
            &Post { forum, views },
        )
        .unwrap();
    }

    let path = dir.path().join("post_reduces.parquet");
    okm_core::bridge::tooling::parquet_io::export_reduces::<_, __OkmReduce_Post_2>(
        t.store(),
        Post::NS_PREFIX,
        "posts",
        &path,
    )
    .unwrap();

    let file = std::fs::File::open(&path).unwrap();
    let batch = ParquetRecordBatchReaderBuilder::try_new(file)
        .unwrap()
        .build()
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let schema = batch.schema();
    assert_eq!(batch.num_columns(), 4);
    assert_eq!(schema.field(0).name(), "table");
    assert_eq!(schema.field(1).name(), "reduce");

    let table = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    assert_eq!(table.value(0), "posts");

    let reduce = batch
        .column(1)
        .as_any()
        .downcast_ref::<UInt16Array>()
        .unwrap();
    assert_eq!(reduce.value(0), <__OkmReduce_Post_2 as Reduce>::SLOT);

    // Two groups (forum 1 and 2): the watermark of forum 1 is 40, of 2 it is 7.
    let mut rows = Vec::new();
    let groups = batch
        .column(2)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .unwrap();
    let values = batch
        .column(3)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .unwrap();
    for i in 0..batch.num_rows() {
        rows.push((
            u64::from_be_bytes(groups.value(i).try_into().unwrap()),
            u64::from_be_bytes(values.value(i).try_into().unwrap()),
        ));
    }
    rows.sort();
    assert_eq!(rows, vec![(1, 40), (2, 7)]);
    assert!(groups.null_count() == 0 && values.null_count() == 0);
}

/// The ledger follows the ROW (ADR-0035): a patch naming no declared field writes
/// only the segment and folds nothing, and a patch writing BOTH halves in one
/// call folds ONCE — the fold hangs off the row arm, not off the map's size.
#[test]
fn a_merge_folds_the_ledger_once_and_only_through_the_row_arm() {
    use okm_core::model::obj_dynamic::DynamicValue;
    use std::collections::BTreeMap;

    let t = <Post as Document>::collection(TestStore::slatedb_mem());
    let k = PostKey { id: 1 };
    t.put(
        &k,
        &Post {
            forum: 7,
            views: 100,
        },
    )
    .unwrap();
    let ns = <Post as Document>::NS_PREFIX;
    let group = 7u64.to_be_bytes();
    let acc = |v: Vec<(Vec<u8>, u64)>| {
        v.into_iter()
            .find(|(sfx, _)| sfx.as_slice() == group.as_slice())
            .map(|(_, a)| a)
    };
    let count = |store: &TestStore| {
        acc(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_0>(store, ns).unwrap())
    };
    let sum = |store: &TestStore| {
        acc(okm_core::reduce::scan_reduces::<_, __OkmReduce_Post_1>(store, ns).unwrap())
    };
    assert_eq!(count(t.store()), Some(1));

    // Segment-only patch: the open half gains a name, the row (and the ledger)
    // is not touched.
    let mut open = BTreeMap::new();
    open.insert("note".to_string(), DynamicValue::Str("x".into()));
    t.merge(&k, &open).unwrap();
    assert_eq!(
        count(t.store()),
        Some(1),
        "a segment-only patch folds nothing"
    );
    assert_eq!(
        t.fetch(&k).unwrap().unwrap().get("note"),
        Some(&DynamicValue::Str("x".into()))
    );

    // Both halves in ONE call: the row arm folds (the usual unfold-then-fold
    // pair), the segment arm does not — one fold, not one per arm.
    let mut both = BTreeMap::new();
    both.insert("views".to_string(), DynamicValue::UInt(7));
    both.insert("tag".to_string(), DynamicValue::Str("y".into()));
    t.merge(&k, &both).unwrap();
    assert_eq!(
        count(t.store()),
        Some(1),
        "one merge folds once, not once per arm"
    );
    assert_eq!(
        sum(t.store()),
        Some(7),
        "the Sum followed the row's new value"
    );
    assert_eq!(
        t.fetch(&k).unwrap().unwrap().get("tag"),
        Some(&DynamicValue::Str("y".into()))
    );
}
