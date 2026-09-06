//! 引擎抽象：同步 KvEngine trait + MockStore 参考实现。

/// KV 引擎最小接口（前缀扫描返回每个 key 的"剩余段"）。
/// fjall / slatedb feature 各自提供实现；测试用 MockStore。
pub trait KvEngine {
    fn put(&mut self, key: Vec<u8>);
    fn del(&mut self, key: &[u8]);
    /// 前缀扫描，返回每个 key 的"剩余段"（去掉 prefix）
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>>;
}

/// 测试/开发用内存引擎（BTreeMap，memcmp 序与真实引擎一致）
#[derive(Default, Clone)]
pub struct MockStore {
    pub keys: std::collections::BTreeMap<Vec<u8>, ()>,
}

impl KvEngine for MockStore {
    fn put(&mut self, key: Vec<u8>) {
        self.keys.insert(key, ());
    }
    fn del(&mut self, key: &[u8]) {
        self.keys.remove(key);
    }
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.keys
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k[prefix.len()..].to_vec())
            .collect()
    }
}
