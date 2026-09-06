//! 组装点：Collection<S, E> —— 引擎 + 边类型 = 一条关系的操作面。

use crate::edge::KvEdge;
use crate::engine::KvEngine;
use crate::key::{KeyEncode, PrefixKey};

/// 引擎 S + 边 E = 一条关系的操作面
pub struct Collection<S, E> {
    pub store: S,
    _pd: std::marker::PhantomData<E>,
}

impl<S: KvEngine, E: KvEdge> Collection<S, E> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            _pd: std::marker::PhantomData,
        }
    }

    /// 原子双写：正向 + 反向
    pub fn link(&mut self, a: &E::A, b: &E::B) {
        let e = E::from_parts(a.clone(), b.clone());
        let fk = e.forward_key();
        let rk = e.reverse_key();
        self.store.put(fk);
        self.store.put(rk);
    }
    pub fn unlink(&mut self, a: &E::A, b: &E::B) {
        let e = E::from_parts(a.clone(), b.clone());
        self.store.del(&e.forward_key());
        self.store.del(&e.reverse_key());
    }

    /// 扫描前缀 = 头部 ++ A·身份（丢弃收尾残留位所在的部分字节——
    /// A·身份之后的部分属于任意 B，不参与匹配）。
    fn forward_prefix(a: &E::A) -> Vec<u8> {
        let mut p = Vec::with_capacity(2 + E::a_head_width());
        p.extend_from_slice(&crate::edge::head_bytes(E::NS, false));
        E::encode_a_head(&mut p, a);
        p
    }

    /// A → Bs：正向扫描。要求 B 全量身份（可 decode）。
    pub fn forward(&self, a: &E::A) -> Vec<E::B> {
        assert!(
            E::B_HEAD.is_empty(),
            "forward 需要 B 全量身份才能 decode 回类型"
        );
        let p = Self::forward_prefix(a);
        self.store
            .scan_suffix(&p)
            .iter()
            .map(|suffix| E::B::decode(suffix))
            .collect()
    }

    /// B → As 的原始前缀字节（A 为截断身份时无法 decode，交回调用方用于主表 scan）
    pub fn reverse_raw(&self, b: &E::B) -> Vec<Vec<u8>> {
        let mut p = Vec::with_capacity(2 + E::b_head_width());
        p.extend_from_slice(&crate::edge::head_bytes(E::NS, true));
        E::encode_b_head(&mut p, b);
        self.store.scan_suffix(&p)
    }

    /// B → As：反向扫描，A 为全量身份时 decode 回类型。
    pub fn reverse(&self, b: &E::B) -> Vec<E::A> {
        assert!(
            E::A_HEAD.is_empty(),
            "A 为截断身份时无法 decode，请用 reverse_raw"
        );
        self.reverse_raw(b)
            .iter()
            .map(|sfx| E::A::decode(sfx))
            .collect()
    }

    /// B → As（截断身份产物）：只保证前 `taken` 字节可信。
    pub fn reverse_prefix(&self, b: &E::B) -> Vec<PrefixKey<E::A>> {
        let taken = if E::A_HEAD.is_empty() {
            E::A::KEY_LEN
        } else {
            E::A::prefix_width(E::A_HEAD)
        };
        self.reverse_raw(b)
            .iter()
            .map(|sfx| PrefixKey {
                decoded: E::A::decode(sfx),
                taken,
            })
            .collect()
    }
}
