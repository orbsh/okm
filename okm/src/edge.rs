//! Edge 侧：KvEdge trait 与方向位头部。

use crate::key::KeyEncode;

/// 头部编码：ns 占 u16 低 15 位，最高 1 位 = 方向位（FWD=0，REV=1）。
/// 语义上 ns 空间为 32768（niche 决策，2026-09-06 定案）；2 字节定长头部保住前缀扫描语义。
pub const NS_BITS: u16 = 15;
pub const DIR_BIT: u16 = 1 << NS_BITS; // 0x8000

#[inline]
pub fn head_bytes(ns: u16, rev: bool) -> [u8; 2] {
    (((ns & (DIR_BIT - 1)) << 1) | (rev as u16)).to_be_bytes()
}

/// 双向边契约。A = 第一个字段（起点），B = 第二个字段（终点）。
/// A_HEAD / B_HEAD 由 EdgeEncode 宏按 #[kv_head(...)] 生成（空 = 全量身份）。
pub trait KvEdge: Sized {
    type A: KeyEncode;
    type B: KeyEncode;
    const NS: u16;
    const A_HEAD: &'static [&'static str];
    const B_HEAD: &'static [&'static str];
    fn a(&self) -> &Self::A;
    fn b(&self) -> &Self::B;
    fn from_parts(a: Self::A, b: Self::B) -> Self;

    fn encode_a_head(buf: &mut Vec<u8>, a: &Self::A) -> usize {
        if Self::A_HEAD.is_empty() {
            buf.extend_from_slice(&a.encode());
            Self::A::KEY_LEN
        } else {
            a.encode_prefix_named(buf, Self::A_HEAD)
        }
    }
    fn a_head_width() -> usize {
        if Self::A_HEAD.is_empty() {
            Self::A::KEY_LEN
        } else {
            Self::A::prefix_width(Self::A_HEAD)
        }
    }
    fn encode_b_head(buf: &mut Vec<u8>, b: &Self::B) -> usize {
        if Self::B_HEAD.is_empty() {
            buf.extend_from_slice(&b.encode());
            Self::B::KEY_LEN
        } else {
            b.encode_prefix_named(buf, Self::B_HEAD)
        }
    }
    fn b_head_width() -> usize {
        if Self::B_HEAD.is_empty() {
            Self::B::KEY_LEN
        } else {
            Self::B::prefix_width(Self::B_HEAD)
        }
    }

    /// [头部 2B: ns<<1 | dir][A·身份][B·身份]
    fn forward_key(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(2 + Self::a_head_width() + Self::b_head_width());
        k.extend_from_slice(&head_bytes(Self::NS, false));
        Self::encode_a_head(&mut k, self.a());
        Self::encode_b_head(&mut k, self.b());
        k
    }
    /// [头部 2B: ns<<1 | dir][B·身份][A·身份]
    fn reverse_key(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(2 + Self::a_head_width() + Self::b_head_width());
        k.extend_from_slice(&head_bytes(Self::NS, true));
        Self::encode_b_head(&mut k, self.b());
        Self::encode_a_head(&mut k, self.a());
        k
    }
}
