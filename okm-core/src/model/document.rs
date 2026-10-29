//! Collection — the document assembly point (ADR-0006): binds an engine instance, a
//! key type, and a document type. `put` builds the whole write set — the
//! primary key (slot 0), one entry per access method, embedded children, the
//! releases of the old document's references and the reduce RMWs — into one
//! batch and commits it once, so a semantic write is one engine commit and one
//! crash boundary; cross-ns atomicity is the store instance's
//! boundary, never the Collection's. Index entries derive from the document payload
//! (indexed + includes fields live there), so put and delete are both
//! document-shaped.

use crate::engine::storage::{SharedVirtualStorage, VirtualStorage, scan_suffix_kv};
use crate::model::index::{Document, KvIndex};
use crate::model::key::{KeyEncode, PrefixKey};

/// One document as the byte-level scans hand it over: `(key encoding suffix,
/// Some(payload))` for a row, `(key encoding suffix, None)` for a document that
/// exists only as a segment. Named because three call sites (the raw scan, the
/// Arrow bridge, the snapshot exporter) carry it and a bare
/// `Vec<(Vec<u8>, Option<Vec<u8>>)>` stops being readable.
pub type RawDocument = (Vec<u8>, Option<Vec<u8>>);

pub struct Collection<S, K: KeyEncode, R: Document<Key = K>> {
    store: S,
    /// Monotonic write-batch counter (in-process only, never persisted):
    /// bumped on every put/delete, stamped on emitted subscribe events so
    /// consumers can fold to exact same-table batch boundaries. Resets on
    /// restart — meaningful only within one process lifetime.
    ///
    /// A `Cell` because it is the ONLY mutable state a write touches, and the
    /// write verbs take `&self` (ADR-0035 rule 10): building the whole write
    /// set is `&self` work through a local [`Batch`], so the exclusivity a
    /// merge's read-modify-write needs no longer comes from `&mut self`. The
    /// `Cell` is what keeps that sound — `Cell` is `!Sync`, so a set of writes
    /// cannot be interleaved by another thread holding the same `&Collection`.
    epoch: std::cell::Cell<u64>,
    /// The table's change log (ADR-0045), when the assembly point attached one.
    /// A `RefCell` for the same reason `epoch` is a `Cell`: the write verbs take
    /// `&self`, and the writer is per-collection state. `!Sync` either way, so
    /// two threads cannot interleave writes through one `&Collection`.
    changelog: Option<std::cell::RefCell<crate::changelog::Changelog>>,
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
            epoch: std::cell::Cell::new(0),
            changelog: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// The same assembly point with a change log attached (ADR-0045): the
    /// FALLIBLE constructor, because it is the one that touches the filesystem —
    /// `new` stays I/O-free on purpose (ADR-0043), exactly as it does for the
    /// meta stamp.
    ///
    /// The switch lives here rather than in a derive attribute because it is a
    /// per-instance policy, the same kind of decision as engine choice.
    pub fn with_changelog(
        store: S,
        config: crate::changelog::ChangelogConfig,
    ) -> crate::error::Result<Self> {
        let mut this = Self::new(store);
        let header = this.header();
        let log = crate::changelog::Changelog::open(config, &header)?;
        // The sidecar is the half of the declaration the log cannot carry (types
        // and key layout — the payload is positional and the key has no field
        // boundaries), and this is the one place that knows both the schema and
        // the directory. Absent the `schema-serde` feature the caller writes it.
        #[cfg(feature = "schema-serde")]
        crate::changelog::write_sidecar(
            log.dir(),
            &crate::model::schema::CollectionSchema::of::<K, R>(),
        )?;
        this.changelog = Some(std::cell::RefCell::new(log));
        Ok(this)
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

    /// The table's meta entry (fixed face 0x0F, ADR-0043): `None` when the store
    /// was never stamped. One point read; an engine failure comes back as `Err`.
    pub fn meta(&self) -> crate::error::Result<Option<crate::model::meta::TableMeta>> {
        crate::model::meta::read(&self.store, &self.header())
    }

    /// First-touch verb (ADR-0043): stamp THIS build's payload shape on a fresh
    /// store, or answer the stamp the store already carries.
    ///
    /// Call it once per table at attach/boot — never on the write path (one
    /// point read per call). `Error::LayoutMismatch` means the store was written
    /// in another shape: export it with the build that stamped it, or delete it
    /// (ADR-0042). `new` stays I/O-free on purpose, which is why this is a verb
    /// rather than constructor work.
    pub fn ensure_layout(&self) -> crate::error::Result<crate::model::meta::TableMeta> {
        crate::model::meta::ensure(&self.store, &self.header(), R::LAYOUT_VERSION)
    }

    /// Record one caller-owned name in the meta entry (ADR-0043), stamping the
    /// table first when it is fresh. `version` is the runtime's: it is re-stamped
    /// from `R::LAYOUT_VERSION` after the caller's name is applied, so the shape
    /// stamp cannot be edited away through this verb.
    ///
    /// Read-modify-write of ONE singleton key, committed as a single write.
    pub fn set_meta(
        &self,
        name: &str,
        value: Vec<u8>,
    ) -> crate::error::Result<crate::model::meta::TableMeta> {
        let mut meta = crate::model::meta::ensure(&self.store, &self.header(), R::LAYOUT_VERSION)?;
        meta.set(name, value);
        meta.set(crate::model::meta::VERSION, vec![R::LAYOUT_VERSION]);
        crate::model::meta::write(&self.store, &self.header(), &meta)?;
        Ok(meta)
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
    pub fn put(&self, key: &K, document: &R) -> crate::error::Result<()> {
        // ONE engine commit per semantic write. The whole write set — primary
        // entry, every declared face, embedded children with their own faces,
        // the releases of children the old document no longer points at, and
        // the reduce RMWs — accumulates in one `Batch` and reaches the
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
        let mut batch = crate::engine::storage::Batch::default();
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
                copy.__okm_intern(&mut store, &header)?;
                interned = Some(copy);
            }
            let document = interned.as_ref().unwrap_or(document);
            // Overwrite detection doubles as the unfold source: the stored
            // document (if any) is exactly what the reduce groups currently
            // include for this key.
            let prev = store.get(&pkey)?.map(|v| R::decode_payload(&v));
            if let Some(old) = &prev {
                R::__okm_apply_reduces(&mut store, key, old, &header, false)?;
            }
            store.put(pkey, document.encode_payload())?;
            for (ek, ev) in R::index_entries(key, document, &header) {
                store.put(ek, ev)?;
            }
            // Embedded documents: write children carried with Some(value), and
            // release references the OLD document pointed at that the new one
            // no longer does (key change). Reference semantics — no cascade.
            for (ck, cp) in R::__okm_embed_entries(document) {
                store.put(ck, cp)?;
            }
            if let Some(old) = &prev {
                let new_keys: std::collections::HashSet<Vec<u8>> =
                    R::__okm_embed_keys(document).into_iter().collect();
                for old_key in R::__okm_embed_keys(old) {
                    if !new_keys.contains(&old_key) {
                        store.del(&old_key)?;
                    }
                }
            }
            // Cross-document reduces: fold this document into each declared
            // group.
            R::__okm_apply_reduces(&mut store, key, document, &header, true)?;
        }
        self.commit(batch)?;
        R::__okm_emit_event(
            crate::subscribe::Op::Put,
            self.epoch.get(),
            key,
            interned.as_ref().unwrap_or(document),
        );
        Ok(())
    }

    /// Commanded RMW: `get` → `f(old)` → `put(key, new)` through the
    /// normal write path, so index maintenance, reduce hooks and channel
    /// emission all fire without special-casing. Returns the written document.
    /// `f` receives `None` when the key has no document yet (insert path).
    ///
    /// Single-writer only: OKM is an in-process library with one owner per
    /// collection and a serial write order, so get→f→put cannot interleave —
    /// no CAS needed. Same constraint that backs reduce's exactly-once. The
    /// order is not `&mut self`'s any more (writes take `&self`, see
    /// [`put`](Self::put)); what keeps it is `Collection` being `!Sync` — the
    /// epoch counter is a `Cell` — so no second owner can hold the same
    /// collection while a write is in flight.
    /// Cross-process exclusion is the engine's job, not OKM's: fjall
    /// takes a file lock on open (a second process fails to open the
    /// same directory), slatedb fences with a writer epoch in the
    /// manifest (a stale writer's commits are rejected). With the engine
    /// arbitrating, any moment has at most one live writer — the
    /// single-writer model holds end to end, and an OKM-level lock would
    /// only double-guard what the engine already enforces. The
    /// multi-writer future (optimistic CAS) triggers only if multiple
    /// live writers over one store become a real requirement.
    pub fn upsert_with(&self, key: &K, f: impl FnOnce(Option<R>) -> R) -> crate::error::Result<R> {
        let new = f(self.get(key)?);
        self.put(key, &new)?;
        Ok(new)
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
    pub fn save_into(&self, batch: &mut crate::engine::storage::Batch, key: &K, document: &R) {
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
        batch: &mut crate::engine::storage::Batch,
        key: &K,
        document: &R,
        prev: Option<&R>,
    ) -> crate::error::Result<()> {
        let pkey = self.primary_key(key);
        let header = self.header();

        let mut adapter = crate::engine::storage::BatchStore::new(&self.store, batch);

        // The old row: the caller's, or read through the adapter so a pending
        // write of the SAME key in this batch counts as the old row.
        let mut prev_owned = match prev {
            Some(_) => None,
            None => adapter.get(&pkey)?.map(|v| R::decode_payload(&v)),
        };
        // A decoded row carries ids, not labels: name it, so a reduce group on a
        // categorical field folds and unfolds under the same key `put` uses.
        if let Some(old) = prev_owned.as_mut() {
            old.__okm_deref_vocab(&self.store)?;
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
            copy.__okm_intern(&mut adapter, &header)?;
            Some(copy)
        } else {
            None
        };
        let document = interned.as_ref().unwrap_or(document);

        // Unfold the old document from every reduce group before folding the
        // new one in — without this the second write double-counts.
        if let Some(old) = prev {
            R::__okm_apply_reduces(&mut adapter, key, old, &header, false)?;
        }

        adapter.put(pkey, document.encode_payload())?;

        // Faces: write the new set, drop the old set's leftovers. An entry
        // key present in both sets holds the same (key, value) pair, so it is
        // written once and never deleted.
        let new_faces = R::index_entries(key, document, &header);
        if let Some(old) = prev {
            let fresh: std::collections::HashSet<&Vec<u8>> =
                new_faces.iter().map(|(ek, _)| ek).collect();
            for (ek, _) in R::index_entries(key, old, &header) {
                if !fresh.contains(&ek) {
                    adapter.del(&ek)?;
                }
            }
        }
        for (ek, ev) in new_faces {
            adapter.put(ek, ev)?;
        }

        // Embedded documents: write children carried with Some(value), and
        // release references the OLD document held that the new one does not.
        for (ck, cp) in R::__okm_embed_entries(document) {
            adapter.put(ck, cp)?;
        }
        if let Some(old) = prev {
            let new_keys: std::collections::HashSet<Vec<u8>> =
                R::__okm_embed_keys(document).into_iter().collect();
            for old_key in R::__okm_embed_keys(old) {
                if !new_keys.contains(&old_key) {
                    adapter.del(&old_key)?;
                }
            }
        }

        // Fold the new document into each declared group.
        R::__okm_apply_reduces(&mut adapter, key, document, &header, true)?;
        Ok(())
    }

    /// Index entry key for access method `I` derived from `key` + `document`.
    pub fn index_key<I: KvIndex<Key = K, Document = R>>(&self, key: &K, document: &R) -> Vec<u8> {
        I::entry_key(&self.header(), key, document)
    }

    /// Point lookup: decode key payload + TLV payload.
    pub fn get(&self, key: &K) -> crate::error::Result<Option<R>> {
        let k = self.primary_key(key);
        let v = match self.store.get(&k)? {
            Some(v) => v,
            None => return Ok(None),
        };
        let mut document = R::decode_payload(&v);
        R::__okm_embed_deref(&mut document, &self.store)?;
        R::__okm_deref_vocab(&mut document, &self.store)?;
        Ok(Some(document))
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
    pub fn scan<I: KvIndex<Key = K, Document = R>>(
        &self,
        encoded: &[u8],
    ) -> crate::error::Result<Vec<(PrefixKey<K>, Option<R>)>> {
        let mut out = Vec::new();
        for pk in crate::scan_index::<S, I>(&self.store, &self.header(), encoded)? {
            let document = if pk.taken == K::KEY_LEN {
                self.get(&pk.decoded)?
            } else {
                None
            };
            out.push((pk, document));
        }
        Ok(out)
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
    ) -> crate::error::Result<Vec<(PrefixKey<K>, Option<R>)>> {
        let mut out = Vec::new();
        for pk in crate::scan_index_exact::<S, I>(&self.store, &self.header(), encoded)? {
            let document = if pk.taken == K::KEY_LEN {
                self.get(&pk.decoded)?
            } else {
                None
            };
            out.push((pk, document));
        }
        Ok(out)
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
    ) -> crate::error::Result<Vec<(PrefixKey<K>, Option<R>)>> {
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
        let mut out = Vec::new();
        for full in self.store.scan_range(&b, e.as_deref())? {
            let suffix = &full[b.len()..];
            if suffix.len() < taken {
                continue;
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
                self.get(&decoded)?
            } else {
                None
            };
            out.push((PrefixKey { decoded, taken }, document));
        }
        Ok(out)
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
    ) -> impl DoubleEndedIterator<Item = crate::error::Result<(PrefixKey<K>, Option<R>)>>
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
            .filter_map(move |item| {
                let (full, _value) = match item {
                    Ok(kv) => kv,
                    Err(e) => return Some(Err(e)),
                };
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
                    match store.get(&k) {
                        Ok(Some(v)) => {
                            let mut document = R::decode_payload(&v);
                            if let Err(e) = R::__okm_embed_deref(&mut document, &store) {
                                return Some(Err(e));
                            }
                            if let Err(e) = R::__okm_deref_vocab(&mut document, &store) {
                                return Some(Err(e));
                            }
                            Some(document)
                        }
                        Ok(None) => None,
                        Err(e) => return Some(Err(e)),
                    }
                } else {
                    None
                };
                Some(Ok((PrefixKey { decoded, taken }, document)))
            })
    }

    /// Covered scan over access method `I`: the includes segment lives in
    /// the entry value, so a covering index answers without going back to
    /// the primary table (materialized view, ADR-0006). Returns each hit's
    /// decoded covered fields — `I::Covered`, `()` when the declaration
    /// covers nothing.
    ///
    /// This is what a covered scan can do that [`scan`](Self::scan) cannot:
    /// no point read and no full payload decode per hit. `scan` returns whole
    /// documents and therefore fetch-backs by definition; a caller that wants
    /// only the covered fields takes this door and pays one entry-value
    /// decode per hit.
    pub fn scan_covered<I: KvIndex<Key = K, Document = R>>(
        &self,
        encoded: &[u8],
    ) -> crate::error::Result<Vec<(PrefixKey<K>, I::Covered)>> {
        let p = I::entry_prefix(&self.header(), encoded);
        let taken = I::key_prefix_width();
        let kl = K::KEY_LEN;
        Ok(scan_suffix_kv(&self.store, &p)?
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
                Some((PrefixKey { decoded, taken }, I::decode_covered(&v)))
            })
            .collect())
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
    pub fn scan_documents_raw(&self) -> crate::error::Result<Vec<RawDocument>> {
        let mut rows = self.header();
        crate::model::index::write_slot(&mut rows, crate::model::index::PRIMARY_SLOT);
        let mut segs = self.header();
        crate::model::index::write_slot(&mut segs, crate::model::index::DYNAMIC_SLOT);
        let rows = scan_suffix_kv(&self.store, &rows)?;
        let segs = self.store.scan_suffix(&segs)?;
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
        Ok(out)
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
    pub fn prune_deprecated_slots(&self) -> crate::error::Result<usize> {
        let mut batch = crate::engine::storage::Batch::default();
        let mut removed = 0;
        for slot in R::DEPRECATED_SLOTS {
            let mut p = self.header();
            crate::model::index::write_slot(&mut p, *slot);
            for sfx in self.store.scan_suffix(&p)? {
                let mut full = p.clone();
                full.extend_from_slice(&sfx);
                batch.del(&full);
                removed += 1;
            }
        }
        // One commit for the whole sweep: the deletes land together, and the
        // engine's refusal comes back instead of dying inside a per-entry `del`.
        if removed > 0 {
            self.commit(batch)?;
        }
        Ok(removed)
    }

    /// Full-ns scan of primary keys (slot-0 entries only — the same
    /// slot-0 discipline as `scan_documents_raw`; index entries are slots 1+).
    /// This is the ROW mode: a segment with no row is not here. For the document
    /// mode see [`scan_document_keys`](Self::scan_document_keys).
    pub fn scan_keys(&self) -> crate::error::Result<Vec<K>> {
        let mut p = self.header();
        crate::model::index::write_slot(&mut p, crate::model::index::PRIMARY_SLOT);
        let mut out = Vec::new();
        for suffix in self.store.scan_suffix(&p)? {
            // A primary entry is `[header][slot][key]`: shorter than its own
            // key means the stored bytes are not what the declaration says,
            // so this is a CORRUPT store — reported, not asserted away.
            if suffix.len() < K::KEY_LEN {
                return Err(crate::error::Error::Corrupt {
                    what: format!(
                        "a primary entry is {} byte(s), shorter than the {}-byte key ",
                        suffix.len(),
                        K::KEY_LEN
                    ),
                });
            }
            out.push(K::decode(&suffix[suffix.len() - K::KEY_LEN..]));
        }
        Ok(out)
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
    pub fn scan_document_keys(&self) -> crate::error::Result<Vec<K>> {
        let mut rows = self.header();
        crate::model::index::write_slot(&mut rows, crate::model::index::PRIMARY_SLOT);
        let mut segs = self.header();
        crate::model::index::write_slot(&mut segs, crate::model::index::DYNAMIC_SLOT);
        let rows = self.store.scan_suffix(&rows)?;
        let segs = self.store.scan_suffix(&segs)?;
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
        Ok(out)
    }
}

use std::collections::BTreeMap;

use crate::model::obj_dynamic::DynamicValue;
use crate::model::obj_vocab::{FIELD_VOCAB, VocabCache};

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
    pub fn fetch(&self, key: &K) -> crate::error::Result<Option<BTreeMap<String, DynamicValue>>> {
        let row = self.get(key)?;
        let stored = self.slot1_map(key)?;
        if row.is_none() && stored.is_none() {
            return Ok(None);
        }
        let mut out = row.map(|r| r.to_map()).unwrap_or_default();
        if let Some(fields) = stored {
            out.extend(fields);
        }
        Ok(Some(out))
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
    ///
    /// Fallible because the map is CALLER data: a name that matches a declared
    /// field but carries the wrong kind (or a value that does not fit it) comes
    /// back as [`Error`](crate::error::Error) instead of silently defaulting.
    /// The check runs before any op enters the batch, so a refused merge leaves
    /// the store exactly as it was — the dynamic half included.
    pub fn merge(
        &self,
        key: &K,
        object: &BTreeMap<String, DynamicValue>,
    ) -> crate::error::Result<()> {
        let (typed, dynamic) = self.split_names(object);
        let mut batch = crate::engine::storage::Batch::default();
        if !typed.is_empty() {
            let prev = self.get(key)?;
            let mut merged = prev.as_ref().map(|r| r.to_map()).unwrap_or_default();
            merged.extend(typed);
            let document = R::try_from_map(&merged)?;
            self.save_with_prev_into(&mut batch, key, &document, prev.as_ref())?;
        }
        if !dynamic.is_empty() {
            let mut stored = self.slot1_map(key)?.unwrap_or_default();
            stored.extend(dynamic);
            self.slot1_into(&mut batch, key, &stored)?;
        }
        self.commit(batch)
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
    ///
    /// Fallible for the same reason as [`merge`](Self::merge): the map is caller
    /// data, and a declared name that carries the wrong kind is refused before
    /// anything is written.
    pub fn replace(
        &self,
        key: &K,
        object: &BTreeMap<String, DynamicValue>,
    ) -> crate::error::Result<()> {
        let (typed, dynamic) = self.split_names(object);
        let mut batch = crate::engine::storage::Batch::default();
        if typed.is_empty() {
            if let Some(prev) = self.get(key)? {
                self.delete_into(&mut batch, key, &prev)?;
            }
        } else {
            // The stored row is the face-sweep source: without it the previous
            // values' entries would dangle, exactly what `put` is allowed to do
            // and a whole-document verb is not.
            let prev = self.get(key)?;
            let document = R::try_from_map(&typed)?;
            self.save_with_prev_into(&mut batch, key, &document, prev.as_ref())?;
        }
        self.slot1_into(&mut batch, key, &dynamic)?;
        self.commit(batch)
    }

    /// Remove the whole document — both slots — in one commit. `false` = there
    /// was nothing to remove.
    pub fn purge(&self, key: &K) -> crate::error::Result<bool> {
        let prev = self.get(key)?;
        let has_segment = self.store.get(&self.fields_key(key))?.is_some();
        if prev.is_none() && !has_segment {
            return Ok(false);
        }
        let mut batch = crate::engine::storage::Batch::default();
        if let Some(document) = &prev {
            self.delete_into(&mut batch, key, document)?;
        }
        if has_segment {
            batch.del(&self.fields_key(key));
        }
        self.commit(batch)?;
        // Subscribe: deletion event (same best-effort contract as put),
        // stamped with the table's post-write epoch. A segment-only document
        // has no row to hand the event, so it emits nothing.
        if let Some(document) = &prev {
            R::__okm_emit_event(
                crate::subscribe::Op::Delete,
                self.epoch.get(),
                key,
                document,
            );
        }
        Ok(true)
    }

    /// The stored dynamic segment as a name-keyed map. `None` = no entry; a
    /// zero-frame list is never written, so empty and absent are one state.
    /// Crate-visible for the export bridge, which needs the segment WITHOUT the
    /// declared half (the variant column) — the public read is [`fetch`].
    pub(crate) fn slot1_map(
        &self,
        key: &K,
    ) -> crate::error::Result<Option<BTreeMap<String, DynamicValue>>> {
        let raw = match self.store.get(&self.fields_key(key))? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        let mut d = VocabCache::new(FIELD_VOCAB);
        let header = self.header();
        // decode_named resolves nested obj field ids through the same
        // vocabulary — nested maps come back fully name-keyed. The resolver
        // answers ids, so an engine failure is parked and re-raised after the
        // walk (a nested decode has nowhere else to put it).
        let mut failed: Option<crate::error::Error> = None;
        let frames = crate::model::obj_dynamic::decode_named(&raw, &mut |id| match d.name_for(
            &self.store,
            &header,
            id,
        ) {
            Ok(name) => name,
            Err(e) => {
                failed = Some(e);
                None
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        if frames.is_empty() {
            return Ok(None);
        }
        let mut out = BTreeMap::new();
        for f in frames {
            if let Some(name) = d.name_for(&self.store, &header, f.id)? {
                out.insert(name, f.value);
            }
            // Unknown id (vocabulary entry absent): drop the field. Under
            // single-writer this cannot happen; with shared engines it is
            // the same degraded mode the cache documents.
        }
        Ok(Some(out))
    }

    /// Encode the whole slot-1 entry into `batch` (vocabulary growth included);
    /// an empty map removes the entry instead of writing a zero-frame list.
    fn slot1_into(
        &self,
        batch: &mut crate::engine::storage::Batch,
        key: &K,
        variants: &BTreeMap<String, DynamicValue>,
    ) -> crate::error::Result<()> {
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
                crate::model::obj_dynamic::put_frame_named(&mut body, name, value, &mut resolver)?;
            }
        }
        if body.is_empty() {
            store.del(&k)?;
        } else {
            store.put(k, body)?;
        }
        Ok(())
    }

    /// Encode a document's removal into `batch`: primary entry, every face it
    /// produced, and the reduce unfolds (which read through the batch view).
    fn delete_into(
        &self,
        batch: &mut crate::engine::storage::Batch,
        key: &K,
        document: &R,
    ) -> crate::error::Result<()> {
        let header = self.header();
        let mut store = crate::engine::storage::BatchStore::new(&self.store, batch);
        store.del(&self.primary_key(key))?;
        for (ek, _) in R::index_entries(key, document, &header) {
            store.del(&ek)?;
        }
        R::__okm_apply_reduces(&mut store, key, document, &header, false)
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
        let declared: std::collections::HashSet<&str> = R::FIELDS.iter().map(|f| f.name).collect();
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

    /// Commit one semantic write and advance the epoch — fallible, so a verb
    /// that promises a `Result` can hand the engine's refusal up. The error
    /// carries the engine's OWN text, never a generic message that hides it.
    fn commit(&self, batch: crate::engine::storage::Batch) -> crate::error::Result<()> {
        // Capture BEFORE the commit, append AFTER it (ADR-0045 §6): a record must
        // never describe a write the engine refused. `stage` only fills memory,
        // so it cannot fail a write; the append can, and when it does the write
        // is already durable — the failure lands in `changelog_status` instead
        // of turning a committed write into an error.
        if let Some(log) = &self.changelog {
            let _ = log.borrow_mut().stage(&batch.ops);
        }
        self.store.commit_batch(batch)?;
        self.epoch.set(self.epoch.get() + 1);
        if let Some(log) = &self.changelog {
            let _ = log.borrow_mut().commit_staged();
        }
        Ok(())
    }

    /// Commit a caller-built batch through THIS collection, so its changelog
    /// captures the writes (ADR-0045).
    ///
    /// `save_into` hands the caller a batch and the caller used to reach the
    /// store directly, which no collection-level hook can see. That path is the
    /// cross-collection atomic one (ADR-0003), and ADR-0031 §5 already says a
    /// batch writer's events are its own concern — this verb is how it discharges
    /// that.
    ///
    /// A batch may carry another table's keys; a log directory belongs to ONE
    /// header, so this captures the keys under THIS collection's header and drops
    /// the rest. For a batch that writes several tables, use the staged/flush
    /// split instead of this shorthand: every participating collection stages its
    /// own share before the ONE commit ([`Self::stage_capture`]) and appends after
    /// the commit returned Ok ([`Self::flush_capture`]) — the capture follows the
    /// keys to their own logs, and none of it describes a write the engine
    /// refused.
    pub fn commit_batch(&self, batch: crate::engine::storage::Batch) -> crate::error::Result<()> {
        self.commit(batch)
    }

    /// Stage this collection's share of `ops` for capture, WITHOUT appending:
    /// the first half of the staged/flush split for a batch that spans tables.
    ///
    /// Returns how many ops were carried (0 when the batch holds nothing under
    /// this header, and when no log is attached). Staging is memory only, so it
    /// cannot fail a write; a staged frame whose commit never happens is dropped
    /// by the next `stage` call — at-most-once, never a phantom row.
    pub fn stage_capture(&self, ops: &[(Vec<u8>, Option<Vec<u8>>)]) -> usize {
        match &self.changelog {
            Some(log) => log.borrow_mut().stage(ops),
            None => 0,
        }
    }

    /// Append what [`Self::stage_capture`] staged. Call it AFTER the one commit
    /// returned Ok, for every collection that staged.
    pub fn flush_capture(&self) -> crate::error::Result<()> {
        match &self.changelog {
            Some(log) => log.borrow_mut().commit_staged(),
            None => Ok(()),
        }
    }

    /// What the change log owes: the open segment, bytes still buffered, records
    /// dropped while the sink was broken, and the last write failure. `None` when
    /// no changelog is attached.
    pub fn changelog_status(&self) -> Option<crate::changelog::ChangelogStatus> {
        self.changelog.as_ref().map(|log| log.borrow().status())
    }
}
