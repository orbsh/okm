//! The asynchronous mirror of the sync storage contract (ADR-0059).
//!
//! Six methods, the same surface as `okm_core::engine::storage::VirtualStorage`,
//! so a caller can move between the two worlds without learning a second API.
//! It lives here, not in core, because a crate boundary is the only boundary
//! cargo enforces: not depending on this crate is what makes the async face
//! absent from a build.

use okm_core::engine::storage::{Batch, ScanIter, prefix_end};

/// 异步引擎最小接口（与同步 VirtualStorage 对齐，ADR-0027：
/// 对齐的是审计后的同步面——scan_suffix_kv/batch 不在 trait 上）。
/// ADR-0028 relocated it from slatedb_backend: the aligned surface is
/// the engine contract, not a slatedb property — async senders over
/// the wire (`WireClient`) need it with no slatedb in sight.
#[allow(async_fn_in_trait)] // engine contract: impls are in-crate backends and wasm senders; ADR-0010 keeps the surface byte-only
pub trait VirtualStorageAsync {
    /// The async mirror of the sync `put`: `Err` is the engine (or the
    /// transport) refusing the bytes.
    async fn put(&self, key: Vec<u8>, value: Vec<u8>) -> okm_core::error::Result<()>;
    /// The async mirror of the sync `get`: `Ok(None)` is absence, `Err` is the
    /// engine (or the transport) failing to answer.
    async fn get(&self, key: &[u8]) -> okm_core::error::Result<Option<Vec<u8>>>;
    /// The async mirror of the sync `del`: `Ok(())` covers "nothing there".
    async fn del(&self, key: &[u8]) -> okm_core::error::Result<()>;
    /// 前缀扫描，返回每个 key 的"剩余段"（去掉 prefix）
    async fn scan_suffix(&self, prefix: &[u8]) -> okm_core::error::Result<Vec<Vec<u8>>>;
    /// Range scan over FULL keys: `[begin, end)` byte order; None end =
    /// unbounded. The async mirror of the sync trait's `scan_range`.
    async fn scan_range(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> okm_core::error::Result<Vec<Vec<u8>>>;
    /// Async mirror of the sync `scan_range_iter` (ADR-0027). The default
    /// BUFFERS via `scan_range` + per-key `get` (N+1); SlatedbStore
    /// overrides with a ONE-PASS materialization (see impl). Laziness is
    /// deliberately NOT attempted: the sync `ScanIter::next` would need
    /// `block_on`, and the async call site lives INSIDE a runtime —
    /// re-entrant `block_on` panics (module header). ADR-0020 already
    /// ruled the async surface small enough that buffering is harmless.
    async fn scan_range_iter(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        let mut pairs: Vec<okm_core::error::Result<(Vec<u8>, Vec<u8>)>> = Vec::new();
        match self.scan_range(begin, end).await {
            Ok(keys) => {
                for k in keys {
                    match self.get(&k).await {
                        Ok(Some(v)) => pairs.push(Ok((k, v))),
                        Ok(None) => {}
                        Err(e) => {
                            pairs.push(Err(e));
                            break;
                        }
                    }
                }
            }
            Err(e) => pairs.push(Err(e)),
        }
        ScanIter::Buffered(pairs.into_iter())
    }
    /// Async mirror of the sync `commit_batch` (ADR-0027): one engine-level
    /// write over all accumulated ops. Default replays through put/del;
    /// engines with a native batch override (SlatedbStore ships slatedb's
    /// `WriteBatch` as ONE `db.write` + durable wait).
    async fn commit_batch(&self, batch: Batch) -> okm_core::error::Result<()> {
        for (k, v) in batch.ops {
            match v {
                Some(v) => self.put(k, v).await?,
                None => self.del(&k).await?,
            }
        }
        Ok(())
    }
}

/// Prefix scan returning `(key suffix, value)` pairs — the async twin of
/// the free `scan_suffix_kv` (ADR-0027; one pair-scan shape in both
/// worlds). Relocated with the trait (ADR-0028).
pub async fn scan_suffix_kv_async<S: VirtualStorageAsync + ?Sized>(
    store: &S,
    prefix: &[u8],
) -> okm_core::error::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let end = prefix_end(prefix);
    let mut out = Vec::new();
    for item in store.scan_range_iter(prefix, end.as_deref()).await {
        let (full, v) = item?;
        out.push((full.get(prefix.len()..).unwrap_or(&[]).to_vec(), v));
    }
    Ok(out)
}
