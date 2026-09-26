//! fjall engine adapter: `FjallStore` = Database + keyspace wrapper,
//! implementing the sync `VirtualStorage`.
//!
//! One OKM Edge corresponds to one keyspace; ns prefixes come with the
//! key encoding itself, so different edge types sharing a keyspace do not
//! conflict.

use crate::engine::storage::{VirtualStorage, MemBatch};
use fjall::{Database, Keyspace, KeyspaceCreateOptions};

/// Clone is a handle clone: Arc-inner in fjall, clones share the keyspace.
#[derive(Clone)]
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

/// Clone IS a shared handle (Arc-inner) — the NestStorage requirement.
impl crate::engine::storage::SharedVirtualStorage for FjallStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}

impl VirtualStorage for FjallStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        self.ks.insert(key, value).expect("fjall insert failed");
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.ks
            .get(key)
            .expect("fjall get failed")
            .map(|v| v.to_vec())
    }
    fn del(&self, key: &[u8]) {
        self.ks.remove(key).expect("fjall remove failed");
    }
    /// fjall native: the op list executes in fjall's own cross-keyspace
    /// `Batch` — one `Batch::commit()` = one real WAL write; the carrier
    /// stays engine-agnostic (MemBatch), the atomicity is fjall's.
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

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        match end {
            Some(end) => self
                .ks
                .range(begin..end)
                .filter_map(|g| g.key().ok())
                .map(|k| k.to_vec())
                .collect(),
            None => self
                .ks
                .range(begin..)
                .filter_map(|g| g.key().ok())
                .map(|k| k.to_vec())
                .collect(),
        }
    }

    /// fjall's `Iter` is an owned, 'static lazy iterator (snapshot
    /// nonce held inside) — native streaming, no buffering, both
    /// directions.
    fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> super::storage::ScanIter {
        let it = match end {
            Some(end) => self.ks.range(begin..end),
            None => self.ks.range(begin..),
        };
        super::storage::ScanIter::Fjall(it)
    }
}
