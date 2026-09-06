//! Key 侧：KeyEncode trait 与 PrefixKey。

/// 定宽 key 的编码契约。
/// KEY_LEN / FIELD_WIDTHS / encode 均由 #[derive(KeyEncode)] 生成；
/// encode_prefix_named 是「截断身份」原语：按声明序编码前 N 个字段。
pub trait KeyEncode: Sized + Clone {
    /// 全量编码字节数（payload，不含 ns）
    const KEY_LEN: usize;
    /// 字段名 → 宽度表（声明序）。edge 宏靠它在运行期校验 kv_head 是合法前缀
    const FIELD_WIDTHS: &'static [(&'static str, usize)];
    fn encode(&self) -> Vec<u8>;
    fn decode(bytes: &[u8]) -> Self;
    /// 按声明序编码前 names.len() 个字段；names 必须是声明序前缀（首个错位即 panic）
    fn encode_prefix_named(&self, buf: &mut Vec<u8>, names: &[&str]) -> usize;
    /// 对应宽度
    fn prefix_width(names: &[&str]) -> usize;
}

/// 截断身份的解码产物：A 的全量 decode + 前缀消耗字节数。
/// 前缀字段可信，后续字段是同 key 内的垃圾（来自 B 的编码区边界误读）——
/// 调用方应只使用前缀字段，或拿 `decoded` 的前缀部分去主表 scan。
pub struct PrefixKey<A> {
    pub decoded: A,
    /// 前缀部分消耗的字节数（= kv_head 各字段宽度之和）
    pub taken: usize,
}
