use okm_core::{KeyEncode, DocumentEncode, Document, Collection, obj_dynamic::DynamicValue};
use parquet_variant::VariantBuilder;
use std::collections::BTreeMap;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
pub struct User {
    pub level: u32,
    pub name: String,
}

#[test]
fn variant_build_smoke() {
    // Build one Variant object from a DynamicValue tree (the shape
    // get_fields returns).
    let mut obj: BTreeMap<String, DynamicValue> = BTreeMap::new();
    obj.insert("score".into(), DynamicValue::UInt(99));
    obj.insert("tags".into(), DynamicValue::Array(vec![
        DynamicValue::Str("a".into()),
        DynamicValue::Str("b".into()),
    ]));
    obj.insert("nested".into(), DynamicValue::Obj(BTreeMap::from([
        ("x".into(), DynamicValue::Int(-3)),
        ("flag".into(), DynamicValue::Bool(true)),
    ])));

    fn push<B: parquet_variant::VariantBuilderExt>(b: &mut B, v: &DynamicValue) {
        match v {
            DynamicValue::Null => b.append_null(),
            DynamicValue::Bool(x) => b.append_value(*x),
            DynamicValue::UInt(x) => b.append_value(*x as i64),
            DynamicValue::Int(x) => b.append_value(*x),
            DynamicValue::F64(x) => b.append_value(*x),
            DynamicValue::Str(sv) => b.append_value(sv.as_str()),
            DynamicValue::Bytes(bv) => b.append_value(bv.as_slice()),
            DynamicValue::Array(items) => {
                // ListBuilder impls VariantBuilderExt — recurse directly.
                let mut lb = b.new_list();
                for it in items { push(&mut lb, it); }
                lb.finish();
            }
            DynamicValue::Obj(map) => {
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
        map: &BTreeMap<String, DynamicValue>,
    ) {
        for (k, val) in map {
            match val {
                DynamicValue::Obj(sub_map) => {
                    let mut sub = ob.new_object(k);
                    push_obj(&mut sub, sub_map);
                    sub.finish();
                }
                DynamicValue::Array(items) => {
                    let mut sub = ob.new_list(k);
                    for it in items { push(&mut sub, it); }
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
    push(&mut b, &DynamicValue::Obj(obj));
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
