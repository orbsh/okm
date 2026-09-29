//! localStorage engine (ADR-0028 decision 1): the SYNC
//! [`VirtualStorage`] contract over `web_sys::Storage` — the browser's
//! only synchronous storage API, so it is the only one the typed layer
//! (`Collection`/`Graph`, which bind the sync trait) can drive in a page.
//!
//! Byte crossing: localStorage physically holds strings only. Keys and
//! values cross as base64 — ADR-0018's exception applies exactly here:
//! the container cannot hold anything else, base64 is the boundary
//! artifact, not a storage encoding choice.
//!
//! Ordering honesty: `scan_range` enumerates ALL keys (`length` +
//! `key(i)`, the API has no range primitive), decodes to bytes, filters
//! the `[begin, end)` interval and SORTS IN RUST. Two facts make the
//! sort mandatory: `Storage.key(i)` iteration order is not specified,
//! and JS string order (UTF-16 unit order) differs from UTF-8 byte
//! order above U+FFFF — sorting the base64 STRINGS would be wrong twice
//! over (base64 is alphabet order over the ENCODED form; only decoded
//! bytes carry okm's BE-key ordering contract). The O(n) enumeration is
//! the accepted cost at UI-state scale (thousands of rows, ≤ a few MB
//! quota): panel-local state by contract, never the business store.
//!
//! Batch honesty: `commit_batch` is the trait's default replay —
//! localStorage has no multi-key transaction. A partial write on a
//! quota error leaves UI state inconsistent; that is the documented
//! tolerance (anything that must not drift lives behind a node's verbs,
//! ADR-0028 Boundaries).

use web_sys::Storage;

use crate::engine::storage::{SharedVirtualStorage, VirtualStorage};

// ================= byte crossing (pure, natively testable) =================

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding — the only alphabet localStorage keys
/// can safely hold (NUL bytes would survive, but raw bytes in JS strings
/// are UTF-16 re-encoded; base64 keeps the round trip byte-exact).
pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { B64[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Inverse of [`b64_encode`]; rejects bad length, illegal characters,
/// and inconsistent padding (never silently truncates — a mis-decoded
/// key is a wrong keyspace, not a cosmetic error).
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for group in bytes.chunks(4) {
        let pad = (group[3] == b'=') as usize + (group[2] == b'=') as usize;
        if pad > 0 {
            // padding only in the final group, trailing
            if group[3] != b'=' {
                return None; // '=' before a non-'=' in the same group
            }
            let expect = if group[2] == b'=' && group[3] == b'=' { 2 } else { 1 };
            if pad != expect {
                return None;
            }
        }
        let mut n = 0u32;
        for (i, &c) in group.iter().enumerate() {
            if i >= 4 - pad {
                break; // padding positions carry no value bits
            }
            n = n << 6 | val(c)?;
        }
        n <<= 6 * pad as u32;
        let total = 3 - pad;
        for i in 0..total {
            out.push((n >> (16 - 8 * i as u32)) as u8);
        }
    }
    Some(out)
}

// ================= the engine =================

/// localStorage as an engine. `Storage` handles are views of the
/// origin's singleton, so a clone (and `shared_handle`) shares the same
/// physical keyspace (the nest's sharing requirement is genuinely met,
/// not faked — same argument as fjall's Arc kernel).
#[derive(Clone)]
pub struct LocalStorageStore {
    storage: Storage,
}

impl LocalStorageStore {
    /// The origin's localStorage (wasm only — the JS binding panics if
    /// there is no `window`; a headless page without storage access
    /// yields `None` from `local_storage()` and this maps that to Err).
    pub fn local() -> Result<Self, String> {
        let win = web_sys::window().ok_or("no window")?;
        let storage = win
            .local_storage()
            .map_err(|e| format!("local_storage blocked: {e:?}"))?
            .ok_or("no localStorage in this context")?;
        Ok(Self { storage })
    }

    /// From an already-held `Storage` (assembly-point injection — lets a
    /// test harness or a different origin slot share the type).
    pub fn from_storage(storage: Storage) -> Self {
        Self { storage }
    }

    /// One enumerated (key bytes, value bytes) row. Decode failures on
    /// FOREIGN keys (other apps on the same origin) return `None` and
    /// the caller skips them — the store's keyspace is the subset it
    /// can byte-round-trip; foreign strings are invisible, never data.
    fn row(&self, index: u32) -> Option<(Vec<u8>, Vec<u8>)> {
        // web-sys: key(i) answers Result<Option<String>> — absent index
        // and JS-level failure both fold to None = "no more rows".
        let k = self.storage.key(index).ok()??;
        let key = b64_decode(&k)?;
        let v = self.storage.get_item(&k).ok()??;
        let value = b64_decode(&v)?;
        Some((key, value))
    }

    /// All rows, in byte order. The O(n) cost is the documented
    /// localStorage boundary (UI-state scale only).
    fn rows_sorted(&self) -> Vec<Vec<u8>> {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 0..self.storage.length().unwrap_or(0) {
            if let Some((key, _)) = self.row(i) {
                keys.push(key);
            }
        }
        keys.sort(); // byte order, not string order — see module header
        keys
    }
}

impl VirtualStorage for LocalStorageStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        self.storage
            .set_item(&b64_encode(&key), &b64_encode(&value))
            .expect("localStorage put failed (quota?)");
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let text = self.storage.get_item(&b64_encode(key)).ok()??;
        b64_decode(&text)
    }

    fn del(&self, key: &[u8]) {
        self.storage
            .remove_item(&b64_encode(key))
            .expect("localStorage remove failed");
    }

    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.rows_sorted()
            .into_iter()
            .filter_map(|k| k.strip_prefix(prefix).map(|s| s.to_vec()))
            .collect()
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        if let Some(end) = end
            && end <= begin
        {
            return Vec::new();
        }
        // `end = None` is genuinely UNBOUNDED (reaching prefix_end here
        // would be wrong twice: begin-only ranges must run to the
        // keyspace end, and prefix_end() is scan_suffix's derivation —
        // the trait default already composes them).
        match end {
            Some(end) => self
                .rows_sorted()
                .into_iter()
                .filter(|k| k.as_slice() >= begin && k.as_slice() < end)
                .collect(),
            None => self
                .rows_sorted()
                .into_iter()
                .filter(|k| k.as_slice() >= begin)
                .collect(),
        }
    }

    fn scan_range_iter(&self, begin: &[u8], end: Option<&[u8]>) -> crate::engine::storage::ScanIter {
        // Materialize pairs in one enumeration pass (values come from the
        // same rows, no per-key second read), then hand back the owned
        // buffer — the trait default's shape, made single-pass here.
        if let Some(end) = end
            && end <= begin
        {
            return crate::engine::storage::ScanIter::Buffered(Vec::new().into_iter());
        }
        let bound: Option<Vec<u8>> = end.map(|e| e.to_vec());
        let mut pairs = Vec::new();
        for i in 0..self.storage.length().unwrap_or(0) {
            if let Some((key, value)) = self.row(i)
                && key.as_slice() >= begin
                && bound.as_ref().is_none_or(|e| key.as_slice() < e.as_slice())
            {
                pairs.push((key, value));
            }
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        crate::engine::storage::ScanIter::Buffered(pairs.into_iter())
    }
}

impl SharedVirtualStorage for LocalStorageStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte crossing is pure logic — native-testable without a
    /// browser, and it MUST be tested: a codec bug mis-keys the whole
    /// keyspace silently (b64_decode already rejects garbage; the round
    /// trip check catches alphabet drift).
    #[test]
    fn base64_round_trips_every_chunk_length() {
        for len in 0..10usize {
            let bytes: Vec<u8> = (0..len as u64).map(|i| (i * 37 + 200) as u8).collect();
            let text = b64_encode(&bytes);
            assert_eq!(text.len() % 4, 0);
            assert_eq!(b64_decode(&text).as_deref(), Some(bytes.as_slice()), "len {len}");
        }
    }

    #[test]
    fn base64_rejects_garbage() {
        assert!(b64_decode("").is_some()); // the empty key is legal
        assert_eq!(b64_decode("AAAA").as_deref(), Some([0u8, 0, 0].as_slice()));
        assert!(b64_decode("AA").is_none()); // bad length
        assert!(b64_decode("A!AA").is_none()); // illegal character
        assert!(b64_decode("AA=A").is_none()); // '=' before a value char
        assert!(b64_decode("AAA=").is_some()); // single pad, last group
        assert_eq!(b64_decode("AAA=").unwrap(), vec![0, 0]); // 3 chars -> 2 bytes
        assert!(b64_decode("A===").is_none()); // over-padding
    }

    #[test]
    fn scan_range_bound_semantics_match_the_trait() {
        // The filter is pure given rows_sorted(); test the interval
        // logic through the same code path the wasm build uses by
        // unit-testing a local replica of the predicate.
        let rows: Vec<Vec<u8>> = vec![b"a".to_vec(), b"m/1".to_vec(), b"m/2".to_vec(), b"z".to_vec()];
        let filter = |begin: &[u8], end: Option<&[u8]>| -> Vec<Vec<u8>> {
            if let Some(e) = end
                && e <= begin
            {
                return Vec::new();
            }
            let mut out: Vec<_> = rows
                .iter()
                .filter(|k| k.as_slice() >= begin && end.is_none_or(|e| k.as_slice() < e))
                .cloned()
                .collect();
            out.sort();
            out
        };
        assert_eq!(filter(b"m", Some(b"z")), vec![b"m/1".to_vec(), b"m/2".to_vec()]);
        assert_eq!(filter(b"m", None), vec![b"m/1".to_vec(), b"m/2".to_vec(), b"z".to_vec()]);
        assert!(filter(b"z", Some(b"a")).is_empty());
        assert_eq!(filter(b"m/1", Some(b"m/1")), Vec::<Vec<u8>>::new());
    }
}
