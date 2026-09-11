use okm_core::{KeyEncode, RowEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(1)]
struct K {
    id: u64,
}

// func(...) 与 fields 混用：排序段只能有一个来源。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(K)]
#[kv_index(bad { fields(name), func(lower) })]
struct BadFuncFields {
    name: String,
}

fn lower(_r: &BadFuncFields) -> String {
    String::new()
}

fn main() {}
