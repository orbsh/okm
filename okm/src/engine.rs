//! Engine abstraction: sync [`KvEngine`] trait + [`MockStore`] reference
//! implementation.

/// Minimal KV engine interface (prefix scan returns the "suffix" of each key).
/// The `fjall` / `slatedb` features each provide an implementation; tests
/// use [`MockStore`].
pub trait KvEngine {
    fn put(&mut self, key: Vec<u8>);
    fn del(&mut self, key: &[u8]);
    /// Prefix scan; returns each matching key's "suffix" (prefix removed).
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>>;
}

/// In-memory engine for tests and development (`BTreeMap`; memcmp order
/// matches real engines).
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
