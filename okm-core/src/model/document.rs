//! Collection — the document assembly point (ADR-0006): binds an engine instance, a
//! key type, and a document type. `put` writes the primary key (slot 0) and one
//! entry per access method in the same store instance, so atomicity holds
//! within a single engine; cross-ns atomicity is the store instance's
//! boundary, never the Collection's. Index entries derive from the document payload
//! (indexed + includes fields live there), so put and delete are both
//! document-shaped.

use crate::engine::storage::{SharedVirtualStorage, VirtualStorage};
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
    /// `R::NS_PREFIX` (ADR-0002: the ns dictionary is code; ADR-0010:
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

    /// Primary key entry (slot 0): `[0xFF][part 1B] (if declared)[ns 2B][slot 0]
    /// [key payload]`, value = TLV payload of the document. The slot byte keeps
    /// the header uniform with index entries (`[ns 2B][slot 1B]`); slot 0 =
    /// primary, per ADR-0005. The partition segment precedes the ns header
    /// (ADR-0014 §5): workload isolation lives outside ownership scope —
    /// a partition groups tables by compaction profile, a namespace groups
    /// them by owner; the 0xFF escape byte makes partitioned and
    /// unpartitioned keys structurally disjoint. Absent when the document
    /// declares no `#[ok_partition]`.
    pub fn primary_key(&self, key: &K) -> Vec<u8> {
        let mut buf = self.header();
        buf.extend_from_slice(&crate::model::index::PRIMARY_SLOT.to_be_bytes());
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
    /// — the stale entry dangles, which is why `delete` (and
    /// `delete_by_pkey`) exist; the reduce, by contrast, is a mutable
    /// aggregate under one group key and MUST compensate.
    pub fn put(&mut self, key: &K, document: &R) {
        // Overwrite detection doubles as the unfold source: the stored
        // document (if any) is exactly what the reduce groups currently
        // include for this key.
        let pkey = self.primary_key(key);
        let prev = self
            .store
            .get(&pkey)
            .map(|v| R::decode_payload(&v));
        if let Some(old) = &prev {
            let header = self.header();
        R::__okm_apply_reduces(&mut self.store, key, old, &header, false);
        }
        self.store.put(pkey, document.encode_payload());
        for (ek, ev) in R::index_entries(key, document, &self.header()) {
            self.store.put(ek, ev);
        }
        // Embedded documents: write children carried with Some(value), and
        // release references the OLD document pointed at that the new one
        // no longer does (key change). Reference semantics — no cascade.
        for (ck, cp) in R::__okm_embed_entries(document) {
            self.store.put(ck, cp);
        }
        if let Some(old) = &prev {
            let new_keys: std::collections::HashSet<Vec<u8>> =
                R::__okm_embed_keys(document).into_iter().collect();
            for old_key in R::__okm_embed_keys(old) {
                if !new_keys.contains(&old_key) {
                    self.store.del(&old_key);
                }
            }
        }
        // Cross-document reduces: fold this document into each declared group.
        // Same store instance, so the RMW shares the engine's atomicity
        // boundary with the document + index writes.
        let header = self.header();
        R::__okm_apply_reduces(&mut self.store, key, document, &header, true);
        // Subscribe: write-path event into the declared channel
        // (best-effort try_send — full channel drops, never blocks).
        // The event carries the table's post-write epoch (monotonic
        // write-batch boundary, ADR-0008 §5).
        self.epoch += 1;
        R::__okm_emit_event(crate::subscribe::Op::Put, self.epoch, key, document);
        let _ = prev; // kept alive for the unfold above; dropped here
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
    /// folds, subscribe emission, overwrite unfold) — it is the encoding
    /// surface, not the semantic one. Documents that participate in reduce or
    /// subscriptions must go through `put`/`upsert_with`; save_into is
    /// for batch-aligned bulk loads where the consumer settles those
    /// folds itself.
    pub fn save_into(&self, batch: &mut impl crate::engine::storage::KvBatch, key: &K, document: &R) {
        batch.put(self.primary_key(key), document.encode_payload());
        for (ek, ev) in R::index_entries(key, document, &self.header()) {
            batch.put(ek, ev);
        }
    }

    /// Index entry key for access method `I` derived from `key` + `document`.
    pub fn index_key<I: KvIndex<Key = K, Document = R>>(&self, key: &K, document: &R) -> Vec<u8> {
        I::entry_key(&self.header(), key, document)
    }

    /// Delete a document: primary key + all declared index entries, all
    /// derived from the document being removed (the declaration IS the
    /// registry).
    ///
    /// Contract: `document` MUST be the document currently stored under `key` —
    /// the index entries are derived from `document`'s field values while the
    /// primary entry is derived from `key` alone, and a mismatched pair
    /// silently leaves dangling index entries (the KV layer's `del` is a
    /// no-op on absent keys, so nothing errors). The typical sound
    /// source is the `document` just fetched for `key` (get / scan 回表).
    /// When the document is not in hand or its provenance is doubtful, use
    /// [`Self::delete_by_pkey`], which derives both halves from the same
    /// read.
    pub fn delete(&mut self, key: &K, document: &R) {
        self.store.del(&self.primary_key(key));
        for (ek, _) in R::index_entries(key, document, &self.header()) {
            self.store.del(&ek);
        }
        // Unfold from every declared reduce group (single call site —
        // delete_by_pkey reaches here after its internal get).
        let header = self.header();
        R::__okm_apply_reduces(&mut self.store, key, document, &header, false);
        // Subscribe: deletion event (same best-effort contract as put),
        // stamped with the table's post-write epoch.
        self.epoch += 1;
        R::__okm_emit_event(crate::subscribe::Op::Delete, self.epoch, key, document);
    }

    /// Delete by primary key only: fetch the document from the primary table
    /// first, then delegate to [`Self::delete`]. Both halves of the
    /// removal (primary entry + index entries) derive from the same
    /// fetched document, so a mismatched key/document pair is structurally
    /// impossible. No-op (returns false) when the key has no document.
    ///
    /// The fetch costs one primary-table point read; when the caller
    /// already holds the document from a prior get/scan, prefer
    /// [`Self::delete`] with it.
    ///
    /// Not atomic: a concurrent writer between the internal get and the
    /// deletes could still orphan index entries. Sound under the
    /// current single-writer engine; revisit with transactional
    /// backends.
    pub fn delete_by_pkey(&mut self, key: &K) -> bool {
        match self.get(key) {
            Some(document) => {
                self.delete(key, &document);
                true
            }
            None => false,
        }
    }

    /// Point lookup: decode key payload + TLV payload.
    pub fn get(&self, key: &K) -> Option<R> {
        let k = self.primary_key(key);
        let v = self.store.get(&k)?;
        let mut document = R::decode_payload(&v);
        R::__okm_embed_deref(&mut document, &self.store);
        Some(document)
    }

    /// Leftmost-prefix scan over access method `I`, then fetch-back
    /// (回表): decode each entry's key prefix and load its document payload.
    /// With a truncated `key(...)` prefix the decoded keys are partial
    /// (`PrefixKey.taken < KEY_LEN`) — use the trustworthy prefix fields
    /// to continue scanning the main table.
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
            .map(move |(full, _value)| {
                let suffix = &full[b_len..];
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
                    k.extend_from_slice(&crate::model::index::PRIMARY_SLOT.to_be_bytes());
                    k.extend_from_slice(&decoded.encode());
                    let v = store.get(&k);
                    v.map(|v| {
                        let mut document = R::decode_payload(&v);
                        R::__okm_embed_deref(&mut document, &store);
                        document
                    })
                } else {
                    None
                };
                (PrefixKey { decoded, taken }, document)
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
        self.store
            .scan_suffix_kv(&p)
            .into_iter()
            .map(|(suffix, v)| {
                assert!(suffix.len() >= taken, "index entry shorter than key prefix");
                let start = suffix.len() - taken;
                let decoded = if taken == kl {
                    K::decode(&suffix[start..])
                } else {
                    let mut buf = vec![0u8; kl];
                    buf[..taken].copy_from_slice(&suffix[start..]);
                    K::decode(&buf)
                };
                (PrefixKey { decoded, taken }, v)
            })
            .collect()
    }

    /// Raw documents: `(key encoding suffix, TLV payload)` per primary entry,
    /// in key order — the byte-level scan surface the Arrow bridge and
    /// snapshot exporter consume without struct materialization. Slot-0
    /// only: the ns segment also holds index entries (slots 1+), which
    /// are derived state excluded from export (rebuilt deterministically
    /// on import via put).
    pub fn scan_documents_raw(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut prefix = self.header();
        prefix.extend_from_slice(&crate::model::index::PRIMARY_SLOT.to_be_bytes());
        self.store
            .scan_suffix(&prefix)
            .into_iter()
            .filter_map(|suffix| {
                let full = [prefix.as_slice(), suffix.as_slice()].concat();
                let v = self.store.get(&full)?;
                Some((suffix, v))
            })
            .collect()
    }

    /// Full-ns scan of primary keys (slot-0 entries only — the same
    /// slot-0 discipline as `scan_documents_raw`; index entries are slots 1+).
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
            p.extend_from_slice(&slot.to_be_bytes());
            for sfx in self.store.scan_suffix(&p) {
                let mut full = p.clone();
                full.extend_from_slice(&sfx);
                self.store.del(&full);
                removed += 1;
            }
        }
        removed
    }

    pub fn scan_keys(&self) -> Vec<K> {
        let mut p = self.header();
        p.extend_from_slice(&crate::model::index::PRIMARY_SLOT.to_be_bytes());
        self.store
            .scan_suffix(&p)
            .iter()
            .map(|suffix| {
                assert!(suffix.len() >= K::KEY_LEN, "primary entry shorter than key");
                K::decode(&suffix[suffix.len() - K::KEY_LEN..])
            })
            .collect()
    }
}

use std::collections::BTreeMap;

use crate::model::obj_dict::DictCache;
use crate::model::obj_dynamic::DynamicValue;

impl<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>> Collection<S, K, R> {
    /// Dynamic-segment key: `[header][DYNAMIC_SLOT][key payload]` — the
    /// per-document extension entry next to the primary (slot 0), same
    /// skeleton as an index entry with no field segment (ADR-0012).
    fn fields_key(&self, key: &K) -> Vec<u8> {
        let mut buf = self.header();
        buf.extend_from_slice(&crate::model::index::DYNAMIC_SLOT.to_be_bytes());
        buf.extend_from_slice(&key.encode());
        buf
    }

    /// Read the obj's dynamic fields: slot 1 frames decoded to names via
    /// the field-name dictionary. `None` = the obj has no dynamic entry
    /// (distinct from an empty entry, which is also None here — a
    /// zero-frame list is never written).
    pub fn get_fields(&mut self, key: &K) -> Option<BTreeMap<String, DynamicValue>> {
        let raw = self.store.get(&self.fields_key(key))?;
        let mut d = DictCache::default();
        let header = self.header();
        // decode_named resolves nested obj field ids through the same
        // dictionary — nested maps come back fully name-keyed.
        let frames = crate::model::obj_dynamic::decode_named(&raw, &mut |id| {
            d.name_for(&mut self.store, &header, id)
        });
        if frames.is_empty() {
            return None;
        }
        let mut out = BTreeMap::new();
        for f in frames {
            if let Some(name) = d.name_for(&mut self.store, &header, f.id) {
                out.insert(name, f.value);
            }
            // Unknown id (dictionary entry absent): drop the field. Under
            // single-writer this cannot happen; with shared engines it is
            // the same degraded mode the cache documents.
        }
        Some(out)
    }

    /// Replace the obj's dynamic fields wholesale: the map becomes the
    /// entire slot-1 entry (fields absent from `variants` are removed —
    /// this is a whole-entry put, not a per-field merge). First-seen
    /// names allocate ids through the dictionary; one engine batch
    /// carries any dictionary growth plus the entry itself.
    pub fn put_fields(
        &mut self,
        key: &K,
        variants: &BTreeMap<String, DynamicValue>,
    ) {
        let mut d = DictCache::default();
        let header = self.header().clone();
        let store = &mut self.store;
        let mut resolver = |name: &str| d.id_for(store, &header, name);
        let mut body = Vec::new();
        for (name, value) in variants {
            // put_frame_named recurses into nested Obj values, sharing
            // the dictionary (id_for allocates on first sight).
            crate::model::obj_dynamic::put_frame_named(&mut body, name, value, &mut resolver);
        }
        self.epoch += 1;
        let k = self.fields_key(key);
        if body.is_empty() {
            self.store.del(&k);
        } else {
            self.store.put(k, body);
        }
    }

    /// The whole obj as a name-keyed map: declared fields (typed, from
    /// slot 0 via FieldDesc) merged over the dynamic segment (slot 1).
    /// Declared names win by construction — the two paths cannot collide
    /// because the typed side is compile-time and the dictionary only
    /// allocates names it has never seen (a dynamic name equal to a
    /// declared name would have to be allocated first; convention:
    /// callers don't, and `get_fields` exposes any violation).
    pub fn get_document(&mut self, key: &K) -> Option<BTreeMap<String, DynamicValue>> {
        let document = self.get(key)?;
        let mut out = document.to_map();
        if let Some(v) = self.get_fields(key) {
            out.extend(v);
        }
        Some(out)
    }

    /// Whole-obj write: fields whose names match the document struct go to
    /// the typed path (slot 0, `R::decode_payload` + field assignment),
    /// the rest to the dynamic segment. Unknown names allocate (this is
    /// the external-data entry point — unknown fields are normal input,
    /// unlike the typed decoder where they are caller bugs). One call,
    /// two slots, one epoch bump.
    ///
    /// The input IS a map — there is no root-wrapping convention for
    /// scalar/array tops (no synthetic `"_root"` field). Callers holding
    /// a non-object document decide themselves: wrap in a named field,
    /// reject, or split; the storage layer does not guess.
    pub fn put_document(
        &mut self,
        key: &K,
        object: &BTreeMap<String, DynamicValue>,
    ) {
        // Split by declared/undeclared: declared names go through the
        // typed path (slot 0 via from_map + put — full pipeline incl.
        // indexes/reduces/events), the rest to the dynamic segment.
        let declared: std::collections::HashSet<&str> =
            R::FIELDS.iter().map(|f| f.name).collect();
        let typed_names: Vec<&String> = object
            .keys()
            .filter(|n| declared.contains(n.as_str()))
            .collect();
        if !typed_names.is_empty() {
            // Read-modify-write: absent declared fields keep their current
            // values (fresh documents default).
            let mut document = match self.get(key) {
                Some(r) => r,
                None => R::from_map(&BTreeMap::new()), // all-default document
            };
            // from_map over the TYPED subset only: the current document's map
            // (or all-default for a fresh key) overwritten by the given
            // declared fields — absent fields keep their values.
            let mut merged = document.to_map();
            for n in &typed_names {
                if let Some(v) = object.get(*n) {
                    merged.insert((*n).clone(), v.clone());
                }
            }
            document = R::from_map(&merged);
            self.put(key, &document);
        }
        let dynamic: BTreeMap<String, DynamicValue> = object
            .iter()
            .filter(|(n, _)| !declared.contains(n.as_str()))
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        self.put_fields(key, &dynamic);
    }

    /// Drop the dynamic entry (declared fields untouched). True if an
    /// entry existed.
    /// Delete the whole document: slot 0 (primary + index entries) AND
    /// the dynamic segment (slot 1). One semantic action, both slots.
    pub fn delete_document(&mut self, key: &K) -> bool {
        let fields_gone = self.delete_fields(key);
        let primary_gone = self.delete_by_pkey(key);
        fields_gone || primary_gone
    }

    pub fn delete_fields(&mut self, key: &K) -> bool {
        let k = self.fields_key(key);
        if self.store.get(&k).is_some() {
            self.store.del(&k);
            self.epoch += 1;
            true
        } else {
            false
        }
    }
}
