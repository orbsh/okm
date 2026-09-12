//! fjall engine adapter: `FjallStore` = Database + keyspace wrapper,
//! implementing the sync `VirtualStorage`.
//!
//! One OKM EdgeTable corresponds to one keyspace; ns prefixes come with the
//! key encoding itself, so different edge types sharing a keyspace do not
//! conflict.

use crate::storage::{KvBatch, VirtualStorage, MemBatch};
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

impl VirtualStorage for FjallStore {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.ks.insert(key, value).expect("fjall insert failed");
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.ks
            .get(key)
            .expect("fjall get failed")
            .map(|v| v.to_vec())
    }
    fn del(&mut self, key: &[u8]) {
        self.ks.remove(key).expect("fjall remove failed");
    }
    fn batch(&mut self) -> MemBatch {
        MemBatch::default()
    }
    /// fjall native: one `Batch::commit()` = one real WAL write over all
    /// accumulated ops (batch() returns MemBatch for encoding; commit
    /// replays into fjall's own cross-keyspace Batch — the atomicity is
    /// fjall's, the carrier stays engine-agnostic).
    fn commit_batch(&mut self, batch: MemBatch) -> Result<(), String> {
        let mut wb = self.db.batch();
        for (k, v) in &batch.ops {
            match v {
                Some(v) => wb.insert(&self.ks, k.clone(), v.clone()),
                None => wb.remove(&self.ks, k.clone()),
            }
        }
        wb.commit().map_err(|e| e.to_string())
    }
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.ks
            .prefix(prefix)
            .filter_map(|g| g.key().ok())
            .map(|k| k[prefix.len()..].to_vec())
            .collect()
    }
}
