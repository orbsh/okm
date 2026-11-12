// Tests keep the vocabulary (a panicking test is a failing test with a stack
// trace); this file is its own crate, so `cfg(test)` does not reach it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::{DocumentEncode, KeyEncode, okm_value::Variant};
use parquet_variant::VariantBuilder;
use std::collections::BTreeMap;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
pub struct User {
    pub level: u32,
    pub name: String,
}

#[test]
fn variant_build_smoke() {
    // Build one Variant object from a Variant tree (the shape the
    // dynamic segment's frames decode to).
    let mut obj: BTreeMap<String, Variant> = BTreeMap::new();
    obj.insert("score".into(), Variant::U64(99));
    obj.insert(
        "tags".into(),
        Variant::Array(vec![Variant::Str("a".into()), Variant::Str("b".into())]),
    );
    obj.insert(
        "nested".into(),
        Variant::Map(BTreeMap::from([
            ("x".into(), Variant::I64(-3)),
            ("flag".into(), Variant::Bool(true)),
        ])),
    );

    fn push<B: parquet_variant::VariantBuilderExt>(b: &mut B, v: &Variant) {
        match v {
            Variant::Null => b.append_null(),
            Variant::Bool(x) => b.append_value(*x),
            Variant::U8(x) => b.append_value(i64::from(*x)),
            Variant::U16(x) => b.append_value(i64::from(*x)),
            Variant::U32(x) => b.append_value(i64::from(*x)),
            Variant::U64(x) => b.append_value(*x as i64),
            Variant::I64(x) => b.append_value(*x),
            Variant::F64(x) => b.append_value(*x),
            Variant::Str(sv) => b.append_value(sv.as_str()),
            Variant::Bytes(bv) => b.append_value(bv.as_slice()),
            Variant::Array(items) => {
                // ListBuilder impls VariantBuilderExt — recurse directly.
                let mut lb = b.new_list();
                for it in items {
                    push(&mut lb, it);
                }
                lb.finish();
            }
            Variant::Map(map) => {
                // ObjectBuilder does NOT impl VariantBuilderExt — object
                // fields go through per-key sub-builders/field builders.
                let mut ob = b.new_object();
                push_obj(&mut ob, map);
                ob.finish();
            }
        }
    }

    fn push_obj<S: parquet_variant::BuilderSpecificState>(
        ob: &mut parquet_variant::ObjectBuilder<'_, S>,
        map: &BTreeMap<String, Variant>,
    ) {
        for (k, val) in map {
            match val {
                Variant::Map(sub_map) => {
                    let mut sub = ob.new_object(k);
                    push_obj(&mut sub, sub_map);
                    sub.finish();
                }
                Variant::Array(items) => {
                    let mut sub = ob.new_list(k);
                    for it in items {
                        push(&mut sub, it);
                    }
                    sub.finish();
                }
                other => {
                    let mut fb = parquet_variant::ObjectFieldBuilder::new(k, ob);
                    push(&mut fb, other);
                }
            }
        }
    }
    let mut b = VariantBuilder::new();
    push(&mut b, &Variant::Map(obj));
    let (metadata, value) = b.finish();
    let variant = parquet_variant::Variant::try_new(&metadata, &value).unwrap();
    let o = variant.as_object().unwrap();
    assert_eq!(o.get("score"), Some(parquet_variant::Variant::Int64(99)));
    // Bind intermediates: as_object borrows the temporary Variant.
    let nested_v = o.get("nested").unwrap();
    let nested = nested_v.as_object().unwrap();
    assert_eq!(nested.get("x"), Some(parquet_variant::Variant::Int64(-3)));
    let tags_v = o.get("tags").unwrap();
    let tags = tags_v.as_list().unwrap();
    assert_eq!(tags.get(0), Some(parquet_variant::Variant::from("a")));
}
