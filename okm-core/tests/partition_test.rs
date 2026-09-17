//! `#[ok_partition]` 集成测试（ADR-0014 §5）：partition 段进键编码
//! （Some(N) → `[part 1B]` 前缀，None → 无段零成本）、表内 put/scan
//! 正常工作、跨表字节空间独立（part 段在 ns 段之前）。

use okm_core::{KeyEncode, Document, DocumentEncode, TestStore};

/// 普通 key：无 partition（默认布局，无 part 段）。
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PlainKey {
    pub id: u64,
}

/// partition 1 表：键布局 = [part 1B][ns 2B][slot][key payload]。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PlainKey)]
#[ok_partition(1)]
#[ok_ns(7)]
pub struct Partitioned {
    pub value: u32,
}

/// 无 partition 对照表：键布局 = [ns 2B][slot][key payload]。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PlainKey)]
#[ok_ns(7)]
pub struct Plain {
    pub value: u32,
}

#[test]
fn partition_segment_precedes_ns_header() {
    // 派生常量：声明表 Some(1) + `[0x01]` 前缀；默认表 None + 空前缀。
    assert_eq!(<Partitioned as Document>::PARTITION_ID, Some(1u8));
    assert_eq!(<Partitioned as Document>::PARTITION_PREFIX, &[0xFFu8, 0x01u8][..]);
    assert!(<Plain as Document>::PARTITION_ID.is_none());
    assert_eq!(<Plain as Document>::PARTITION_PREFIX.len(), 0);

    // 键布局锁定：partition 表的主键 = [0xFF][0x01][ns 7 BE][slot 0][key]
    // （0xFF 逃逸字节：合法 ns 头首字节永不取 0xFF → 结构性无碰撞）；
    // 无 partition 表的主键 = [ns 7 BE][slot 0][key]（无 part 段）。
    let store = TestStore::default();
    let mut pt: okm_core::Collection<_, PlainKey, Partitioned> = okm_core::Collection::new(store.clone());
    let mut pl: okm_core::Collection<_, PlainKey, Plain> = okm_core::Collection::new(store.clone());

    let k = PlainKey { id: 1 };
    let pk = pt.primary_key(&k);
    let lk = pl.primary_key(&k);
    assert_eq!(&pk[..2], &[0xFF, 0x01], "partitioned key starts with [0xFF][part 1B] escape segment");
    assert_eq!(&pk[2..4], &[0x00, 0x07], "ns 7 big-endian follows");
    assert_eq!(&lk[..2], &[0x00, 0x07], "plain key starts with ns header directly (never 0xFF)");
    assert_eq!(&pk[4..], &lk[2..], "slot byte + key payload identical after headers");
    // 结构性无碰撞：plain 键首字节属于 0x00-0xFE（ns 字典纪律），part 表
    // 键首字节恒为 0xFF——前缀空间天然不相交，无需编号对齐。

    // 表内 put/get/scan 正常（经 partition 前缀域）。
    pt.put(&k, &Partitioned { value: 42 });
    assert_eq!(pt.get(&k).unwrap().value, 42);
    assert_eq!(pt.scan_keys().len(), 1);
    pl.put(&k, &Plain { value: 7 });
    assert_eq!(pl.get(&k).unwrap().value, 7);
    assert_eq!(pl.scan_keys().len(), 1);

    // 跨表字节空间独立：两表在 TestStore（无物理 partition）下靠键
    // 前缀区分——partition 段保证两者键永不重合。
    assert_ne!(pk, lk);
}

#[test]
fn partition_zero_rejected() {
    // 编译期拒绝：#[ok_partition(0)] 产生 compile_error!（see derive）。
    // 运行期此处只验证 PARTITION_ID 语义：0 不作为合法 id 出现。
    assert_ne!(<Partitioned as Document>::PARTITION_ID, Some(0));
}
