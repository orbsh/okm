//! 基础集成测试：槽位族的编译期检查。TestStore 上的读写形状见
//! `relation_table_test.rs`（关系表）与 `save_into_test.rs`（批处理）。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

/// 槽位是位置语义：高半字节是族，低 12 位是计数器。越界值不再被掩码——
/// 掩码会把它落到另一个族里合法-looking 的槽位上（0x3FFF + 1 = 0x4001 是
/// reduce 族），条目会被另一张表读回去。构造即拒绝。
#[test]
#[should_panic(expected = "slot counter out of range")]
fn slot_counter_is_checked_not_masked() {
    let _ = okm_core::index::slot(okm_core::index::segment::INDEX, 0x1000);
}

#[test]
#[should_panic(expected = "slot family out of range")]
fn slot_family_is_checked_not_wrapped() {
    let _ = okm_core::index::slot(0x10, 1);
}
