//! `Vector<T>` — typed variable-length homogeneous list (ADR-0015 §4, P3.5).
//! Frame layout hex lock, cold-segment integration, `#[ok_len]` contract.

use okm_core::{
    DocumentEncode, KeyEncode, TestStore, Collection, Vector, VectorElem,
};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct DocKey {
    pub id: u64,
}

/// Vector 字段走冷段 TLV 帧（不定长），与 String 同落位。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DocKey)]
#[ok_ns(31)]
pub struct Doc {
    pub id: u64,
    pub embed: Vector<f32>,
    pub score: u32,
}

/// `#[ok_len(2)]` 是解码期合同检查（非 wire 约束）。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DocKey)]
#[ok_ns(32)]
pub struct ContractedDoc {
    pub id: u64,
    #[ok_len(2)]
    pub embed: Vector<f32>,
}

#[test]
fn element_codec_contract() {
    // 元素宽度表（LE 合同）
    assert_eq!(<f32 as VectorElem>::ELEM_WIDTH, 4);
    assert_eq!(<u64 as VectorElem>::ELEM_WIDTH, 8);
}

#[test]
fn payload_roundtrip_le() {
    // 帧载荷 = [count u32 BE] + count × f32 LE
    let v: Vector<f32> = Vector::new(vec![0.25, -3.5, 1e6]);
    let p = v.encode_payload();
    assert_eq!(p.len(), 4 + 12);
    assert_eq!(&p[0..4], &3u32.to_be_bytes());
    assert_eq!(&p[4..8], &0.25f32.to_le_bytes());
    assert_eq!(&p[8..12], &(-3.5f32).to_le_bytes());
    assert_eq!(Vector::decode_payload(&p), v);
}

#[test]
fn cold_frame_roundtrip() {
    let mut t: Collection<TestStore, DocKey, Doc> = Collection::new(TestStore::slatedb_mem());
    let k = DocKey { id: 1 };
    let r = Doc {
        id: 1,
        embed: Vector::new(vec![0.25, -3.5, 1e6]),
        score: 7,
    };
    t.put(&k, &r);

    let got = t.get(&k).unwrap();
    assert_eq!(got.embed, Vector::new(vec![0.25, -3.5, 1e6]));
    assert_eq!(got.score, 7);
}

#[test]
fn length_is_data_not_schema() {
    // 同一个文档类型，不同长度的向量都是合法数据（换模型 = 换帧长）。
    let mut t: Collection<TestStore, DocKey, Doc> = Collection::new(TestStore::slatedb_mem());
    t.put(&DocKey { id: 1 }, &Doc { id: 1, embed: Vector::new(vec![1.0]), score: 1 });
    t.put(
        &DocKey { id: 2 },
        &Doc { id: 2, embed: Vector::new(vec![1.0, 2.0, 3.0, 4.0]), score: 2 },
    );
    assert_eq!(t.get(&DocKey { id: 1 }).unwrap().embed.len(), 1);
    assert_eq!(t.get(&DocKey { id: 2 }).unwrap().embed.len(), 4);
}

#[test]
fn ok_len_contract_enforced_at_encode() {
    // #[ok_len(2)]：合同在编码期（写入边界）执行——count != 2 的 put 即 panic；
    // 解码不检查（绕过解码器 = 乱码自负，与直接看图片字节流同理）。
    let bad = std::panic::catch_unwind(|| {
        let mut t: Collection<TestStore, DocKey, ContractedDoc> =
            Collection::new(TestStore::slatedb_mem());
        t.put(
            &DocKey { id: 1 },
            &ContractedDoc { id: 1, embed: Vector::new(vec![1.0, 2.0, 3.0]) },
        );
    });
    assert!(bad.is_err(), "count 3 violates the #[ok_len(2)] contract");
}

#[test]
fn field_contracts_exported() {
    // FIELD_CONTRACTS 进 CollectionSchema（动态 reader 用同一检查）。
    let schema = okm_core::schema::CollectionSchema::of::<DocKey, ContractedDoc>();
    let cold = schema.cold_fields.iter().find(|f| f.name == "embed").unwrap();
    assert_eq!(cold.expect_len, Some(2));
}

#[test]
fn empty_default() {
    let v: Vector<f32> = Vector::default();
    assert!(v.is_empty());
    assert_eq!(v.encode_payload(), 0u32.to_be_bytes().to_vec());
}

#[test]
fn schema_export_field_desc() {
    // FieldDesc: Vector { elem: "f32" }，宽度 0（冷段，变长）
    let fields = <Doc as okm_core::Document>::FIELDS;
    let fd = fields.iter().find(|f| f.name == "embed").unwrap();
    assert_eq!(
        fd.ty,
        okm_core::field::FieldType::Vector { elem: "f32" }
    );
    assert_eq!(fd.width, 0);
}
