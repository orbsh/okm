//! Collection — the document assembly point (ADR-0006): binds an engine instance, a
//! key type, and a document type. `put` builds the whole write set — the
//! primary key (slot 0), one entry per access method, embedded children, the
//! releases of the old document's references and the reduce RMWs — into one
//! batch and commits it once, so a semantic write is one engine commit and one
//! crash boundary; cross-ns atomicity is the store instance's
//! boundary, never the Collection's. Index entries derive from the document payload
//! (indexed + includes fields live there), so put and delete are both
//! document-shaped.

use crate::engine::storage::{scan_suffix_kv, SharedVirtualStorage, VirtualStorage};
use crate::model::index::{KvIndex, Document};
use crate::model::key::{KeyEncode, PrefixKey};

pub struct Collection<S, K: KeyEncode, R: Document<Key = K>> {
    store: S,
    /// Monotonic write-batch counter (in-process only, never persisted):
    /// bumped on every put/delete, stamped on emitted subscribe events so
    /// consumers can fold to exact same-table batch boundaries. Resets on
    /// restart — meaningful only within one process lifetime.
    epoch: u64,
    _marker: std::marker::PhantomData<(K, R)>,
}

impl<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>> Collection<S, K, R> {
    /// The ns prefix is NOT a constructor argument: it is declared once
    /// on the key struct (`#[ok_ns]`) and read at compile time via
    /// `R::NS_PREFIX` (ADR-0002: the ns vocabulary is code; ADR-0010:
    /// engine choice is per-assembly-point, ns is not). The assembly
    /// site picks the engine; it never restates the ns.
    pub fn new(store: S) -> Self {
        Self {
            store,
            epoch: 0,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Mutable store handle for a caller-owned write path that must reach the
    /// engine outside `put`/`save_into` — the Parquet import is the one: it
    /// re-interns labels into the destination vocabulary BEFORE the rows land.
    pub(crate) fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// The table's key header: `[0xFF][part 1B] (if declared)[ns 2B]` — every
    /// key byte sequence this table writes starts with it. ADR-0014 §5:
    /// the partition segment precedes the ns header (workload isolation
    /// lives outside ownership scope); absent when the document declares no
    /// `#[ok_partition]`.
    fn header(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3);
        buf.extend_from_slice(R::PARTITION_PREFIX);
        buf.extend_from_slice(R::NS_PREFIX);
        buf
    }

    /// Primary key entry (fixed face 0x00, one byte):
    /// `[0xFF][part 1B] (if declared)[ns 2B][0x00][key payload]`, value = the
    /// document's TLV payload. The partition segment precedes the ns header
    /// (ADR-0014 §5): workload isolation lives outside ownership scope —
    /// a partition groups tables by compaction profile, a namespace groups
    /// them by owner; the 0xFF escape byte makes partitioned and
    /// unpartitioned keys structurally disjoint. Absent when the document
    /// declares no `#[ok_partition]`.
    pub fn primary_key(&self, key: &K) -> Vec<u8> {
        let mut buf = self.header();
        crate::model::index::write_slot(&mut buf, crate::model::index::PRIMARY_SLOT);
        buf.extend_from_slice(&key.encode());
        buf
    }

    /// Write a document: primary key + one (key, value) index entry per access
    /// method, all derived from this one document.
    ///
    /// Overwrite semantics (ADR-0008 fold/unfold discipline): when the
    /// key already holds a document, the stored document is unfolded from every
    /// reduce group before the new document is folded in — otherwise the
    /// second write double-counts. This costs one primary-table point
    /// read per overwrite (inserts skip it: slot-0 miss = no fold to
    /// undo). Index entries need no counterpart: they are derived
    /// per-document and the entry key encodes the indexed fields, so an
    /// overwrite with different indexed values lands at a different key
    /// — the stale entry dangles, which is why `purge` exists; the reduce,
    /// by contrast, is a mutable
    /// aggregate under one group key and MUST compensate.
    pub fn put(&mut self, key: &K, document: &R) {
        // ONE engine commit per semantic write. The whole write set — primary
        // entry, every declared face, embedded children with their own faces,
        // the releases of children the old document no longer points at, and
        // the reduce RMWs — accumulates in one `MemBatch` and reaches the
        // engine as a single `commit_batch` (ADR-0003's "internal buffer" half:
        // the same batch `save_into` exposes to callers, owned here by the
        // routine path). `BatchStore` is the read-through view that lets a
        // reduce fold see the writes this same batch already produced
        // (ADR-0031), so the op order below is exactly the old call order.
        // Beyond the call count, this makes the write atomic under a crash,
        // not merely under the single writer: the primary entry, its faces and
        // the ledgers land in one engine commit or not at all.
        let header = self.header();
        let pkey = self.primary_key(key);
        let mut batch = crate::engine::storage::MemBatch::default();
        // Categorical values (ADR-0032): a `Vocab<N>` field holds a NAME until
        // the write path interns it, and the payload byte is the id. Tables
        // with no Vocab field hand the caller's document straight through; the
        // clone is the price of `put(&R)`'s immutable signature, paid only by
        // the tables that declare a categorical field. Interning writes into
        // the same batch, so the vocabulary's growth commits with the row.
        let mut interned: Option<R> = None;
        {
            let mut store = crate::engine::storage::BatchStore::new(&self.store, &mut batch);
            if R::HAS_VOCAB {
                let mut copy = document.clone();
                copy.__okm_intern(&mut store, &header);
                interned = Some(copy);
            }
            let document = interned.as_ref().unwrap_or(document);
            // Overwrite detection doubles as the unfold source: the stored
            // document (if any) is exactly what the reduce groups currently
            // include for this key.
            let prev = store.get(&pkey).map(|v| R::decode_payload(&v));
            if let Some(old) = &prev {
                R::__okm_apply_reduces(&mut store, key, old, &header, false);
            }
            store.put(pkey, document.encode_payload());
            for (ek, ev) in R::index_entries(key, document, &header) {
                store.put(ek, ev);
            }
            // Embedded documents: write children carried with Some(value), and
            // release references the OLD document pointed at that the new one
            // no longer does (key change). Reference semantics — no cascade.
            for (ck, cp) in R::__okm_embed_entries(document) {
                store.put(ck, cp);
            }
            if let Some(old) = &prev {
                let new_keys: std::collections::HashSet<Vec<u8>> =
                    R::__okm_embed_keys(document).into_iter().collect();
                for old_key in R::__okm_embed_keys(old) {
                    if !new_keys.contains(&old_key) {
                        store.del(&old_key);
                    }
                }
            }
            // Cross-document reduces: fold this document into each declared
            // group.
            R::__okm_apply_reduces(&mut store, key, document, &header, true);
        }
        self.commit(batch);
        R::__okm_emit_event(
            crate::subscribe::Op::Put,
            self.epoch,
            key,
            interned.as_ref().unwrap_or(document),
        );
    }

    /// Commanded RMW: `get` → `f(old)` → `put(key, new)` through the
    /// normal write path, so index maintenance, reduce hooks and channel
    /// emission all fire without special-casing. Returns the written document.
    /// `f` receives `None` when the key has no document yet (insert path).
    ///
    /// Single-writer only: OKM is an in-process library with a serial
    /// write order (`&mut self`), so get→f→put cannot interleave — no
    /// CAS needed. Same constraint that backs reduce's exactly-once.
    /// Cross-process exclusion is the engine's job, not OKM's: fjall
    /// takes a file lock on open (a second process fails to open the
    /// same directory), slatedb fences with a writer epoch in the
    /// manifest (a stale writer's commits are rejected). With the engine
    /// arbitrating, any moment has at most one live writer — the
    /// single-writer model holds end to end, and an OKM-level lock would
    /// only double-guard what the engine already enforces. The
    /// multi-writer future (optimistic CAS) triggers only if multiple
    /// live writers over one store become a real requirement.
    pub fn upsert_with(&mut self, key: &K, f: impl FnOnce(Option<R>) -> R) -> R {
        let new = f(self.get(key));
        self.put(key, &new);
        new
    }

    /// Encode this document's full write set (primary entry + one entry per
    /// access method) into an externally owned batch — no write happens
    /// until the batch commits. The cross-collection atomic path
    /// (ADR-0003): a document table and an edge table (or two documents tables)
    /// each `save_into` the same batch, then one `commit_batch` makes
    /// them live or die together.
    ///
    /// Note: save_into bypasses put's higher write-path hooks (reduce
    /// folds, subscribe emission, overwrite unfold, stale-face cleanup) —
    /// it is the encoding surface, not the semantic one. A table that
    /// declares `#[ok_reduce]`, or an overwrite that must not leave a
    /// dangling face, goes through [`Self::save_with_prev_into`] (the
    /// semantic sibling, ADR-0031) or the point path `put`/`upsert_with`;
    /// save_into stays for batch-aligned bulk loads where the consumer
    /// settles those folds itself.
    pub fn save_into(&self, batch: &mut crate::engine::storage::MemBatch, key: &K, document: &R) {
        batch.put(self.primary_key(key), document.encode_payload());
        for (ek, ev) in R::index_entries(key, document, &self.header()) {
            batch.put(ek, ev);
        }
    }

    /// The semantic sibling of [`save_into`](Self::save_into) (ADR-0031):
    /// same primary + face encoding into the caller's batch, plus the write
    /// path's semantics that `save_into` leaves to the caller —
    ///
    /// - **reduce fold/unfold**, applied through a
    ///   [`BatchStore`](crate::engine::storage::BatchStore) (a batch
    ///   read-through view) so a table declaring `#[ok_reduce]` can join a
    ///   batch at all, and two writes to one group in one batch accumulate
    ///   instead of each folding from the pre-batch value;
    /// - **faces the new document no longer produces are deleted**: an
    ///   entry key encodes the indexed value, so a changed value lands at a
    ///   new key — the old entry would dangle forever (`put`'s documented
    ///   contract), and this path removes it in the same batch;
    /// - **embedded documents released**, exactly as `put` does: children
    ///   `prev` pointed at that the new document does not are deleted.
    ///
    /// `prev` is the caller's old row when it holds it (the common case: it
    /// just read it); `None` reads it through the adapter, which also sees a
    /// document this same batch has already produced for the key.
    ///
    /// Subscribe emission stays OUT (ADR-0031 §5): a batch writer's events
    /// are its own concern — emitting from an uncommitted batch would
    /// announce writes that may never land.
    pub fn save_with_prev_into(
        &self,
        batch: &mut crate::engine::storage::MemBatch,
        key: &K,
        document: &R,
        prev: Option<&R>,
    ) {
        let pkey = self.primary_key(key);
        let header = self.header();

        let mut adapter = crate::engine::storage::BatchStore::new(&self.store, batch);

        // The old row: the caller's, or read through the adapter so a pending
        // write of the SAME key in this batch counts as the old row.
        let mut prev_owned = match prev {
            Some(_) => None,
            None => adapter.get(&pkey).map(|v| R::decode_payload(&v)),
        };
        // A decoded row carries ids, not labels: name it, so a reduce group on a
        // categorical field folds and unfolds under the same key `put` uses.
        if let Some(old) = prev_owned.as_mut() {
            old.__okm_deref_vocab(&self.store);
        }
        let prev = prev.or(prev_owned.as_ref());

        // Intern through the BATCH (ADR-0032): allocation is a point read of the
        // vocabulary's cursor and a three-write batch, and through `BatchStore`
        // those writes land in the caller's batch — the vocabulary's growth, the
        // row and its faces commit together or not at all. This is what the
        // cursor face bought: the earlier design had to scan for `max + 1`, and
        // a scan is exactly what a batch cannot do.
        let interned: Option<R> = if R::HAS_VOCAB {
            let mut copy = document.clone();
            copy.__okm_intern(&mut adapter, &header);
            Some(copy)
        } else {
            None
        };
        let document = interned.as_ref().unwrap_or(document);

        // Unfold the old document from every reduce group before folding the
        // new one in — without this the second write double-counts.
        if let Some(old) = prev {
            R::__okm_apply_reduces(&mut adapter, key, old, &header, false);
        }

        adapter.put(pkey, document.encode_payload());

        // Faces: write the new set, drop the old set's leftovers. An entry
        // key present in both sets holds the same (key, value) pair, so it is
        // written once and never deleted.
        let new_faces = R::index_entries(key, document, &header);
        if let Some(old) = prev {
            let fresh: std::collections::HashSet<&Vec<u8>> =
                new_faces.iter().map(|(ek, _)| ek).collect();
            for (ek, _) in R::index_entries(key, old, &header) {
                if !fresh.contains(&ek) {
                    adapter.del(&ek);
                }
            }
        }
        for (ek, ev) in new_faces {
            adapter.put(ek, ev);
        }

        // Embedded documents: write children carried with Some(value), and
        // release references the OLD document held that the new one does not.
        for (ck, cp) in R::__okm_embed_entries(document) {
            adapter.put(ck, cp);
        }
        if let Some(old) = prev {
            let new_keys: std::collections::HashSet<Vec<u8>> =
                R::__okm_embed_keys(document).into_iter().collect();
            for old_key in R::__okm_embed_keys(old) {
                if !new_keys.contains(&old_key) {
                    adapter.del(&old_key);
                }
            }
        }

        // Fold the new document into each declared group.
        R::__okm_apply_reduces(&mut adapter, key, document, &header, true);
    }

    /// Index entry key for access method `I` derived from `key` + `document`.
    pub fn index_key<I: KvIndex<Key = K, Document = R>>(&self, key: &K, document: &R) -> Vec<u8> {
        I::entry_key(&self.header(), key, document)
    }

    /// Point lookup: decode key payload + TLV payload.
    pub fn get(&self, key: &K) -> Option<R> {
        let k = self.primary_key(key);
        let v = self.store.get(&k)?;
        let mut document = R::decode_payload(&v);
        R::__okm_embed_deref(&mut document, &self.store);
        R::__okm_deref_vocab(&mut document, &self.store);
        Some(document)
    }

    /// Leftmost-prefix scan over access method `I`, then fetch-back
    /// (回表): decode each entry's key prefix and load its document payload.
    /// With a truncated `key(...)` prefix the decoded keys are partial
    /// (`PrefixKey.taken < KEY_LEN`) — use the trustworthy prefix fields
    /// to continue scanning the main table.
    ///
    /// Prefix semantics: a stored value that EXTENDS the probe matches
    /// (`"alpha"` probes `"alpha"` and `"alphabet"`). Entries whose probe
    /// bytes were satisfied by primary-key tail bytes are dropped, never
    /// returned (`"ad"` + tail `0x31…` vs the probe `"ad1"`) — exact
    /// matching on the value is [`scan_exact`](Self::scan_exact).
    pub fn scan<I: KvIndex<Key = K, Document = R>>(&self, encoded: &[u8]) -> Vec<(PrefixKey<K>, Option<R>)> {
        crate::scan_index::<S, I>(&self.store, &self.header(), encoded)
            .into_iter()
            .map(|pk| {
                let document = if pk.taken == K::KEY_LEN {
                    self.get(&pk.decoded)
                } else {
                    None
                };
                (pk, document)
            })
            .collect()
    }

    /// Equality scan over access method `I`: only entries whose field
    /// segment is exactly `encoded` — exact matching without reading the
    /// row (no fetch-back validation, no `includes` needed). A stored
    /// value that merely EXTENDS the probe is not a hit, and neither is
    /// the tail-spill case (a probe satisfied by primary-key bytes). The
    /// probe must name every field of the index's data segment; a partial
    /// probe is a group query — use [`scan`](Self::scan).
    pub fn scan_exact<I: KvIndex<Key = K, Document = R>>(
        &self,
        encoded: &[u8],
    ) -> Vec<(PrefixKey<K>, Option<R>)> {
        crate::scan_index_exact::<S, I>(&self.store, &self.header(), encoded)
            .into_iter()
            .map(|pk| {
                let document = if pk.taken == K::KEY_LEN {
                    self.get(&pk.decoded)
                } else {
                    None
                };
                (pk, document)
            })
            .collect()
    }

    /// Range scan over access method `I`: `begin`/`end` are the ENCODED
    /// forms of the leading index fields (byte order == value order for
    /// every OKM encoding — BE fixed-width and prefix-monotonic VarInt).
    /// `[begin, end)` on the entry keys, so the engine reads only rows
    /// whose `a` falls inside the interval — the physical WHERE of a
    /// `1 < a < 100` predicate. Semantics beyond the leading-field
    /// interval (an end that cuts into the carried pkey, mixed bound
    /// widths) are the caller's: the bound bytes are spliced between
    /// the entry header and the identity tail, exactly the region
    /// `entry_prefix` fills with an equality prefix.
    pub fn scan_range<I: KvIndex<Key = K, Document = R>>(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> Vec<(PrefixKey<K>, Option<R>)> {
        let header = self.header();
        let mut b = I::entry_prefix(&header, &[]);
        b.extend_from_slice(begin);
        let e = end.map(|end| {
            let mut e = I::entry_prefix(&header, &[]);
            e.extend_from_slice(end);
            e
        });
        let taken = I::key_prefix_width();
        let kl = K::KEY_LEN;
        self.store
            .scan_range(&b, e.as_deref())
            .iter()
            .filter_map(|full| {
                let suffix = &full[b.len()..];
                if suffix.len() < taken {
                    return None;
                }
                let start = suffix.len() - taken;
                let decoded = if taken == kl {
                    I::Key::decode(&suffix[start..])
                } else {
                    let mut buf = vec![0u8; kl];
                    buf[..taken].copy_from_slice(&suffix[start..]);
                    I::Key::decode(&buf)
                };
                let document = if taken == kl {
                    self.get(&decoded)
                } else {
                    None
                };
                Some((PrefixKey { decoded, taken }, document))
            })
            .collect()
    }

    /// Lazy range scan over access method `I`: identical bounds to
    /// [`scan_range`](Self::scan_range), but entries are pulled on
    /// demand — a consumer that stops early (LIMIT, first match) pays
    /// only the reads performed. Documents are fetched per pulled entry,
    /// never for the untouched tail.
    pub fn scan_range_iter<I: KvIndex<Key = K, Document = R>>(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> impl DoubleEndedIterator<Item = (PrefixKey<K>, Option<R>)>
    where
        S: SharedVirtualStorage,
    {
        let header = self.header();
        let mut b = I::entry_prefix(&header, &[]);
        b.extend_from_slice(begin);
        let e = end.map(|end| {
            let mut e = I::entry_prefix(&header, &[]);
            e.extend_from_slice(end);
            e
        });
        let taken = I::key_prefix_width();
        let kl = K::KEY_LEN;
        // Fetch-back needs an engine handle that outlives the iterator
        // borrow: `shared_handle` hands out a view of the SAME physical
        // engine (never a deep copy — forking the keyspace behind two
        // cursors would be unsound), which is exactly the
        // SharedVirtualStorage contract.
        let store = self.store.shared_handle();
        let b_len = b.len();
        self.store
            .scan_range_iter(&b, e.as_deref())
            .filter_map(move |(full, _value)| {
                let suffix = &full[b_len..];
                // Same false-hit test as the prefix scan: a probe whose
                // bytes were satisfied by the primary-key tail is not a
                // match (`suffix` below the tail width means the field
                // segment is SHORTER than the probe).
                if suffix.len() < taken {
                    return None;
                }
                let start = suffix.len() - taken;
                let decoded = if taken == kl {
                    K::decode(&suffix[start..])
                } else {
                    let mut buf = vec![0u8; kl];
                    buf[..taken].copy_from_slice(&suffix[start..]);
                    K::decode(&buf)
                };
                let document = if taken == kl {
                    // Point fetch-back on the shared engine handle:
                    // same key layout as `Collection::get` (header +
                    // slot-0 primary entry), inlined because the
                    // closure cannot re-enter `self`.
                    let mut k = header.clone();
                    crate::model::index::write_slot(&mut k, crate::model::index::PRIMARY_SLOT);
                    k.extend_from_slice(&decoded.encode());
                    let v = store.get(&k);
                    v.map(|v| {
                        let mut document = R::decode_payload(&v);
                        R::__okm_embed_deref(&mut document, &store);
                        R::__okm_deref_vocab(&mut document, &store);
                        document
                    })
                } else {
                    None
                };
                Some((PrefixKey { decoded, taken }, document))
            })
    }

    /// Covered scan over access method `I`: the includes segment lives in
    /// the entry value, so a full-covering index answers without going
    /// back to the primary table (materialized view, ADR-0006). Returns
    /// `(key prefix, entry value bytes)`.
    pub fn scan_covered<I: KvIndex<Key = K, Document = R>>(
        &self,
        encoded: &[u8],
    ) -> Vec<(PrefixKey<K>, Vec<u8>)> {
        let p = I::entry_prefix(&self.header(), encoded);
        let taken = I::key_prefix_width();
        let kl = K::KEY_LEN;
        scan_suffix_kv(&self.store, &p)
            .into_iter()
            .filter_map(|(suffix, v)| {
                // Entries whose probe bytes were satisfied by the tail are
                // dropped here too — the covered path sees the same range.
                if suffix.len() < taken {
                    return None;
                }
                let start = suffix.len() - taken;
                let decoded = if taken == kl {
                    K::decode(&suffix[start..])
                } else {
                    let mut buf = vec![0u8; kl];
                    buf[..taken].copy_from_slice(&suffix[start..]);
                    K::decode(&buf)
                };
                Some((PrefixKey { decoded, taken }, v))
            })
            .collect()
    }

    /// Raw documents: `(key encoding suffix, payload)` per document, in key
    /// order — the byte-level scan surface the Arrow bridge and snapshot
    /// exporter consume without struct materialization. Index entries (slots
    /// 2+) are derived state, rebuilt on import, so they are not here.
    ///
    /// `Some(payload)` is a row (slot 0); `None` is a document that exists only
    /// as a segment (slot 1) — ADR-0035's document is the PAIR of slots, and a
    /// `merge` / `replace` naming no declared field leaves exactly that. The row
    /// half is one prefix scan WITH values, the segment half a key-only scan, and
    /// the union is a two-pointer merge: no point read per document.
    pub fn scan_documents_raw(&self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let mut rows = self.header();
        crate::model::index::write_slot(&mut rows, crate::model::index::PRIMARY_SLOT);
        let mut segs = self.header();
        crate::model::index::write_slot(&mut segs, crate::model::index::DYNAMIC_SLOT);
        let rows = scan_suffix_kv(&self.store, &rows);
        let segs = self.store.scan_suffix(&segs);
        let mut out = Vec::with_capacity(rows.len() + segs.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < rows.len() || j < segs.len() {
            let take_row = match (rows.get(i), segs.get(j)) {
                (Some((r, _)), Some(s)) => r <= s,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_row {
                let (suffix, payload) = &rows[i];
                if segs.get(j) == Some(suffix) {
                    j += 1; // a row and its segment are ONE document
                }
                out.push((suffix.clone(), Some(payload.clone())));
                i += 1;
            } else {
                out.push((segs[j].clone(), None));
                j += 1;
            }
        }
        out
    }

    /// Full-ns scan of primary keys (slot-0 entries only — the same
    /// slot-0 discipline as `scan_documents_raw`; index entries are slots 1+).
    /// This is the ROW mode: a segment with no row is not here. For the document
    /// mode see [`scan_document_keys`](Self::scan_document_keys).
    /// Clear stale entries under deprecated index slots (ADR-0005):
    /// a `#[ok_index(..., deprecated)]` declaration keeps its slot
    /// reserved but writes nothing; entries written before the
    /// deprecation remain until this method deletes them
    /// (`[ns][deprecated slot]` prefix scan, delete each). Returns the
    /// number of entries removed. Idempotent — a second call finds
    /// nothing. Does NOT touch live slots or the primary table.
    pub fn prune_deprecated_slots(&mut self) -> usize {
        let mut removed = 0;
        for slot in R::DEPRECATED_SLOTS {
            let mut p = self.header();
            crate::model::index::write_slot(&mut p, *slot);
            for sfx in self.store.scan_suffix(&p) {
                let mut full = p.clone();
                full.extend_from_slice(&sfx);
                self.store.del(&full);
                removed += 1;
            }
        }
        removed
    }

    /// Full-ns scan of primary keys (slot-0 entries only — the same
    /// slot-0 discipline as `scan_documents_raw`; index entries are slots 1+).
    /// This is the ROW mode: a segment with no row is not here. For the document
    /// mode see [`scan_document_keys`](Self::scan_document_keys).
    pub fn scan_keys(&self) -> Vec<K> {
        let mut p = self.header();
        crate::model::index::write_slot(&mut p, crate::model::index::PRIMARY_SLOT);
        self.store
            .scan_suffix(&p)
            .iter()
            .map(|suffix| {
                assert!(suffix.len() >= K::KEY_LEN, "primary entry shorter than key");
                K::decode(&suffix[suffix.len() - K::KEY_LEN..])
            })
            .collect()
    }

    /// Every document's KEY, in key order: the rows (slot 0) merged with the
    /// segments that have NO row (slot 1). [`scan_keys`](Self::scan_keys) is the
    /// other mode — rows only, one prefix scan, no join.
    ///
    /// Two modes, because a document is the PAIR of slots (ADR-0035): a `merge` /
    /// `replace` carrying no declared name leaves a segment with no row, and that
    /// document is invisible to a slot-0 scan. This one costs a second prefix
    /// scan and an in-memory merge, and no point read at all.
    ///
    /// Materializing is deliberately NOT this method's job — `fetch` is the door,
    /// so a key becomes a document with `.map(|k| collection.fetch(&k))`.
    pub fn scan_document_keys(&self) -> Vec<K> {
        let mut rows = self.header();
        crate::model::index::write_slot(&mut rows, crate::model::index::PRIMARY_SLOT);
        let mut segs = self.header();
        crate::model::index::write_slot(&mut segs, crate::model::index::DYNAMIC_SLOT);
        let rows = self.store.scan_suffix(&rows);
        let segs = self.store.scan_suffix(&segs);
        // The suffix IS the key encoding in both slots, and a scan hands back key
        // order — so the union is a two-pointer merge: no set, no sort, no read.
        let mut out = Vec::with_capacity(rows.len() + segs.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < rows.len() || j < segs.len() {
            let take_row = match (rows.get(i), segs.get(j)) {
                (Some(r), Some(s)) => r <= s,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_row {
                if segs.get(j) == rows.get(i) {
                    j += 1; // a row and its segment are ONE document
                }
                out.push(K::decode(&rows[i]));
                i += 1;
            } else {
                out.push(K::decode(&segs[j]));
                j += 1;
            }
        }
        out
    }
}

use std::collections::BTreeMap;

use crate::model::obj_vocab::{VocabCache, FIELD_VOCAB};
use crate::model::obj_dynamic::DynamicValue;

impl<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>> Collection<S, K, R> {
    /// Dynamic-segment key: `[header][0x01][key payload]` — the per-document
    /// extension entry next to the primary (fixed face 0x00), same skeleton as
    /// an index entry with no field segment (ADR-0012; fixed face per
    /// ADR-0030).
    fn fields_key(&self, key: &K) -> Vec<u8> {
        let mut buf = self.header();
        crate::model::index::write_slot(&mut buf, crate::model::index::DYNAMIC_SLOT);
        buf.extend_from_slice(&key.encode());
        buf
    }

    /// The document as a name-keyed map — the read door of the whole-document
    /// surface, paired with `merge` / `replace` / `purge` (ADR-0035). Declared
    /// fields (typed, from slot 0) come first, the dynamic segment's names
    /// (slot 1) extend them. Declared names win by construction — the typed
    /// side is compile-time and the vocabulary only allocates names it has
    /// never seen.
    ///
    /// `None` = nothing stored in EITHER slot. A dynamic-only document (no
    /// primary entry) still fetches: "the document" is the pair of slots, not
    /// the primary entry. `get` is the other read door — it speaks `R`.
    pub fn fetch(&self, key: &K) -> Option<BTreeMap<String, DynamicValue>> {
        let row = self.get(key);
        let stored = self.slot1_map(key);
        if row.is_none() && stored.is_none() {
            return None;
        }
        let mut out = row.map(|r| r.to_map()).unwrap_or_default();
        if let Some(fields) = stored {
            out.extend(fields);
        }
        Some(out)
    }

    /// Partial whole-document update, ONE engine commit.
    ///
    /// - A name that matches a declared field is a read-modify-write of the row:
    ///   the stored document is decoded, the named fields overwrite their slots
    ///   in its map, and the row is rewritten through the semantic path — so
    ///   faces the PREVIOUS values produced are removed. (`put` deliberately
    ///   leaves that cleanup to `purge`; a whole-document entry point
    ///   has no old row in hand, so it cleans.)
    /// - Any other name is a dynamic field: it overwrites that name in slot 1
    ///   and leaves the other dynamic names alone.
    ///
    /// Declared fields the map does NOT name keep their stored values. Use
    /// [`replace`](Self::replace) when the map is meant to be the whole document.
    pub fn merge(&mut self, key: &K, object: &BTreeMap<String, DynamicValue>) {
        let (typed, dynamic) = self.split_names(object);
        let mut batch = crate::engine::storage::MemBatch::default();
        if !typed.is_empty() {
            let prev = self.get(key);
            let mut merged = prev.as_ref().map(|r| r.to_map()).unwrap_or_default();
            merged.extend(typed);
            let document = R::from_map(&merged);
            self.save_with_prev_into(&mut batch, key, &document, prev.as_ref());
        }
        if !dynamic.is_empty() {
            let mut stored = self.slot1_map(key).unwrap_or_default();
            stored.extend(dynamic);
            self.slot1_into(&mut batch, key, &stored);
        }
        self.commit(batch);
    }

    /// Whole-document replacement, ONE engine commit: the map IS the document.
    ///
    /// - Declared names rebuild the row; a declared field the map does not name
    ///   takes the struct's default (the payload is positional — a half row
    ///   cannot be encoded). The row's previous faces are removed.
    /// - A map with NO declared name leaves the static segment absent, deleting
    ///   the row if it existed. The dynamic segment is exactly the map's
    ///   non-declared names, so `replace(key, &empty_map)` is
    ///   [`purge`](Self::purge).
    pub fn replace(&mut self, key: &K, object: &BTreeMap<String, DynamicValue>) {
        let (typed, dynamic) = self.split_names(object);
        let mut batch = crate::engine::storage::MemBatch::default();
        if typed.is_empty() {
            if let Some(prev) = self.get(key) {
                self.delete_into(&mut batch, key, &prev);
            }
        } else {
            // The stored row is the face-sweep source: without it the previous
            // values' entries would dangle, exactly what `put` is allowed to do
            // and a whole-document verb is not.
            let prev = self.get(key);
            let document = R::from_map(&typed);
            self.save_with_prev_into(&mut batch, key, &document, prev.as_ref());
        }
        self.slot1_into(&mut batch, key, &dynamic);
        self.commit(batch);
    }

    /// Remove the whole document — both slots — in one commit. `false` = there
    /// was nothing to remove.
    pub fn purge(&mut self, key: &K) -> bool {
        let prev = self.get(key);
        let has_segment = self.store.get(&self.fields_key(key)).is_some();
        if prev.is_none() && !has_segment {
            return false;
        }
        let mut batch = crate::engine::storage::MemBatch::default();
        if let Some(document) = &prev {
            self.delete_into(&mut batch, key, document);
        }
        if has_segment {
            batch.del(&self.fields_key(key));
        }
        self.commit(batch);
        // Subscribe: deletion event (same best-effort contract as put),
        // stamped with the table's post-write epoch. A segment-only document
        // has no row to hand the event, so it emits nothing.
        if let Some(document) = &prev {
            R::__okm_emit_event(crate::subscribe::Op::Delete, self.epoch, key, document);
        }
        true
    }

    /// The stored dynamic segment as a name-keyed map. `None` = no entry; a
    /// zero-frame list is never written, so empty and absent are one state.
    /// Crate-visible for the export bridge, which needs the segment WITHOUT the
    /// declared half (the variant column) — the public read is [`fetch`].
    pub(crate) fn slot1_map(&self, key: &K) -> Option<BTreeMap<String, DynamicValue>> {
        let raw = self.store.get(&self.fields_key(key))?;
        let mut d = VocabCache::new(FIELD_VOCAB);
        let header = self.header();
        // decode_named resolves nested obj field ids through the same
        // vocabulary — nested maps come back fully name-keyed.
        let frames = crate::model::obj_dynamic::decode_named(&raw, &mut |id| {
            d.name_for(&self.store, &header, id)
        });
        if frames.is_empty() {
            return None;
        }
        let mut out = BTreeMap::new();
        for f in frames {
            if let Some(name) = d.name_for(&self.store, &header, f.id) {
                out.insert(name, f.value);
            }
            // Unknown id (vocabulary entry absent): drop the field. Under
            // single-writer this cannot happen; with shared engines it is
            // the same degraded mode the cache documents.
        }
        Some(out)
    }

    /// Encode the whole slot-1 entry into `batch` (vocabulary growth included);
    /// an empty map removes the entry instead of writing a zero-frame list.
    fn slot1_into(
        &self,
        batch: &mut crate::engine::storage::MemBatch,
        key: &K,
        variants: &BTreeMap<String, DynamicValue>,
    ) {
        let header = self.header();
        let k = self.fields_key(key);
        let mut d = VocabCache::new(FIELD_VOCAB);
        let mut store = crate::engine::storage::BatchStore::new(&self.store, batch);
        let mut body = Vec::new();
        {
            let mut resolver = |name: &str| d.id_for(&mut store, &header, name);
            for (name, value) in variants {
                // put_frame_named recurses into nested Obj values, sharing the
                // vocabulary (id_for allocates on first sight).
                crate::model::obj_dynamic::put_frame_named(&mut body, name, value, &mut resolver);
            }
        }
        if body.is_empty() {
            store.del(&k);
        } else {
            store.put(k, body);
        }
    }

    /// Encode a document's removal into `batch`: primary entry, every face it
    /// produced, and the reduce unfolds (which read through the batch view).
    fn delete_into(
        &self,
        batch: &mut crate::engine::storage::MemBatch,
        key: &K,
        document: &R,
    ) {
        let header = self.header();
        let mut store = crate::engine::storage::BatchStore::new(&self.store, batch);
        store.del(&self.primary_key(key));
        for (ek, _) in R::index_entries(key, document, &header) {
            store.del(&ek);
        }
        R::__okm_apply_reduces(&mut store, key, document, &header, false);
    }

    /// Split a name-keyed map along the declaration: declared names are the
    /// static segment, the rest the dynamic one.
    fn split_names(
        &self,
        object: &BTreeMap<String, DynamicValue>,
    ) -> (
        BTreeMap<String, DynamicValue>,
        BTreeMap<String, DynamicValue>,
    ) {
        let declared: std::collections::HashSet<&str> =
            R::FIELDS.iter().map(|f| f.name).collect();
        let mut typed = BTreeMap::new();
        let mut dynamic = BTreeMap::new();
        for (name, value) in object {
            if declared.contains(name.as_str()) {
                typed.insert(name.clone(), value.clone());
            } else {
                dynamic.insert(name.clone(), value.clone());
            }
        }
        (typed, dynamic)
    }

    /// Commit one semantic write and advance the epoch. The engine's error has
    /// no channel in this API, so it surfaces as a panic carrying the engine's
    /// own text (never a generic message that hides it).
    fn commit(&mut self, batch: crate::engine::storage::MemBatch) {
        if let Err(e) = self.store.commit_batch(batch) {
            panic!("okm: commit_batch failed: {e}");
        }
        self.epoch += 1;
    }
}
