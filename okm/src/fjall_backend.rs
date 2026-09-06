//! fjall 引擎适配：`FjallStore` = Database + keyspace 包装，实现同步 `KvEngine`。
//!
//! 一个 OKM Collection 对应一个 keyspace；ns 前缀由 key 编码自带，
//! 不同边类型共享 keyspace 也不冲突。

use crate::engine::KvEngine;
use fjall::{Database, Keyspace, KeyspaceCreateOptions};

pub struct FjallStore {
    db: Database,
    ks: Keyspace,
}

impl FjallStore {
    /// 打开（或创建）path 下的 keyspace `name`
    pub fn open(path: &std::path::Path, name: &str) -> fjall::Result<Self> {
        let db = Database::create_or_recover(fjall::Config::new(path))?;
        let ks = db.keyspace(name, KeyspaceCreateOptions::default)?;
        Ok(Self { db, ks })
    }

    /// 从已存在的 Database 挂一个 keyspace
    pub fn from_db(db: Database, name: &str) -> fjall::Result<Self> {
        Ok(Self {
            ks: db.keyspace(name, KeyspaceCreateOptions::default)?,
            db,
        })
    }

    /// 持久化（fjall 默认 Buffer 模式，需要时手动刷）
    pub fn persist(&self) -> fjall::Result<()> {
        self.db.persist(fjall::PersistMode::SyncData)
    }
}

impl KvEngine for FjallStore {
    fn put(&mut self, key: Vec<u8>) {
        self.ks.insert(key, []).expect("fjall insert failed");
    }
    fn del(&mut self, key: &[u8]) {
        self.ks.remove(key).expect("fjall remove failed");
    }
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.ks
            .prefix(prefix)
            .filter_map(|g| g.key().ok())
            .map(|k| k[prefix.len()..].to_vec())
            .collect()
    }
}
