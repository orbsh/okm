use okm_core::{KeyEncode, RowEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(2)]
struct K2 {
    id: u64,
}

// 变长字段（String）之后的定宽字段无法定位（无静态宽度），编译期拒绝。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(K2)]
#[kv_index(bad_order { fields(name, city) })]
struct BadVarPos {
    name: String,
    city: u32,
}

fn main() {}
