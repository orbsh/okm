use okm_core::{KeyEncode, DocumentEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(1)]
struct K {
    id: u64,
}

// func(...) 与 fields 混用：排序段只能有一个来源。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_index(bad { fields(name), func(lower) })]
struct BadFuncFields {
    name: String,
}

fn lower(_r: &BadFuncFields) -> String {
    String::new()
}

fn main() {}
