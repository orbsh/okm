use okm_core::{DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(3)]
struct K {
    id: u64,
}

// ADR-0055：函数索引的返回形态是 IntoIterator。单值要包成单元素数组
// （`[String; 1]`）——裸 `String` 不迭代，声明处就该停住，而不是在
// 写第一条时才发现段里没有东西。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(401)]
#[ok_index(by_name { func(lower) })]
struct Doc {
    name: String,
}

fn lower(row: &Doc) -> String {
    row.name.to_lowercase()
}

fn main() {}