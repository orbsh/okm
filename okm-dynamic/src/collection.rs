//! DynamicCollection — a runtime-typed table facade over VirtualStorage.
//!
//! The typed `Collection<S, K, R>` binds document/key types at compile time; this
//! facade binds them at runtime through a `CollectionSchema` + declared
//! `AccessMethod`s. Put/get/delete/scan produce and consume the same
//! bytes as the derive (codec shared with the typed path, byte equality
//! locked by the cross tests).
//!
//! Single-writer only (same contract as `Collection`): writes take `&self`,
//! so one owner per table — `_owned` carries a `Cell` marker and keeps this
//! type `!Sync`. The engine arbitrates cross-process exclusion.
//!
//! Capability scope (ADR-0022): subscribe stays excluded; reduce and
//! function/partial indexes are binding-implementable via host-language
//! callables under the deployment-shape contract (embedded: in-process
//! calling discipline; remote: operation payloads carry semantic
//! results, single-writer-per-group).

use okm_core::storage::VirtualStorage;

use crate::index::{AccessMethod, scan_access_method};
use crate::reduce::{BoundReduce, ReduceSpec};
use crate::{CollectionSchema, ValueMap, decode_payload};

/// Codec errors surfaced as strings (dynamic callers are host-language
/// bridges — error values, not typed hierarchies).
fn codec<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// One planned write op: `(key, Some(value))` = put, `(key, None)` =
/// delete — the op shape the whole plan surface uses.
pub(crate) type EngineOp = (Vec<u8>, Option<Vec<u8>>);

/// A runtime-declared table: schema + access methods + a namespaced
/// slot of storage. `ns` MUST be unique within the store instance —
/// one keyspace, one ns vocabulary. Keeping allocations apart is the
/// deployment's duty (aura reserves the framework bands and allocates
/// booth types above them, ADR-0040), never a runtime check.
pub struct DynamicCollection<S: VirtualStorage> {
    pub(crate) store: S,
    pub(crate) schema: CollectionSchema,
    pub(crate) ns: Vec<u8>,
    pub(crate) indexes: Vec<AccessMethod>,
    pub(crate) reduces: Vec<BoundReduce>,
    /// Single-writer by construction, not by convention: `!Sync`, so a
    /// `&DynamicCollection` cannot be handed to a second thread while a
    /// read-modify-write is in flight (the typed `Collection` gets the same
    /// property from its `Cell` epoch counter).
    _owned: std::marker::PhantomData<std::cell::Cell<()>>,
}

impl<S: VirtualStorage> DynamicCollection<S> {
    /// Declare a dynamic table. `ns` is the 2-byte BE namespace segment
    /// (the table's own allocation; access methods share it, slot bytes
    /// discriminate within). Slots on the access methods are caller-
    /// allocated (1-based, unique per table) — mirroring declaration
    /// order in the typed path.
    pub fn new(store: S, ns: u16, schema: CollectionSchema, indexes: Vec<AccessMethod>) -> Self {
        Self::with_reduces(store, ns, schema, indexes, Vec::new())
    }

    /// Declare a dynamic table with reduce groups (ADR-0022). Slots on
    /// the reduces are caller-allocated (unique per table, mirroring the
    /// derive's declaration-order rule in the reduce segment).
    pub fn with_reduces(
        store: S,
        ns: u16,
        schema: CollectionSchema,
        indexes: Vec<AccessMethod>,
        reduces: Vec<ReduceSpec>,
    ) -> Self {
        let slots: Vec<u16> = indexes.iter().map(|i| i.slot).collect();
        debug_assert!(
            slots.iter().all(|s| *s > 0) && {
                let mut sorted = slots.clone();
                sorted.sort_unstable();
                sorted.dedup();
                sorted.len() == slots.len()
            },
            "access method slots must be unique and 1-based"
        );
        let rslots: Vec<u16> = reduces.iter().map(|r| r.slot).collect();
        debug_assert!(
            rslots.iter().all(|s| *s > 0) && {
                let mut sorted = rslots.clone();
                sorted.sort_unstable();
                sorted.dedup();
                sorted.len() == rslots.len()
            },
            "reduce slots must be unique and 1-based"
        );
        Self {
            store,
            schema,
            ns: ns.to_be_bytes().to_vec(),
            indexes,
            reduces: reduces.into_iter().map(BoundReduce::new).collect(),
            _owned: std::marker::PhantomData,
        }
    }

    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// Direct store access (tests, maintenance sweeps).
    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn indexes(&self) -> &[AccessMethod] {
        &self.indexes
    }

    /// The table's meta entry (fixed face 0x0F, ADR-0043) — the SAME row the
    /// typed `Collection` stamps, addressed by this table's ns. `None` = the
    /// store was never stamped; an engine failure surfaces as its message (this
    /// crate answers dynamic callers, which carry no typed error hierarchy).
    pub fn meta(&self) -> Result<Option<okm_core::TableMeta>, String> {
        okm_core::meta::read(&self.store, &self.ns).map_err(codec)
    }

    /// First-touch verb (ADR-0043) on the dynamic side: stamp a fresh store
    /// with `schema.layout_version`, or answer the stamp it already carries.
    ///
    /// The typed counterpart is `Collection::ensure_layout`, and both call
    /// `okm_core::meta::ensure` — `CollectionSchema::layout_version` IS
    /// `Document::LAYOUT_VERSION`, so a store stamped by either mode satisfies
    /// the other. One point read when stamped, one point write when not: call
    /// it where the host attaches to a store, never on the write path.
    ///
    /// A store stamped with another shape (or a meta row naming no version) is
    /// refused with `Error::LayoutMismatch`'s message, which names both numbers.
    pub fn ensure_layout(&self) -> Result<okm_core::TableMeta, String> {
        okm_core::meta::ensure(&self.store, &self.ns, self.schema.layout_version).map_err(codec)
    }

    /// Record one caller-owned name beside the stamp, re-stamping `version`
    /// afterwards (ADR-0043: the shape stamp belongs to the runtime, so a
    /// caller cannot edit it away through this verb).
    pub fn set_meta(&self, name: &str, value: Vec<u8>) -> Result<okm_core::TableMeta, String> {
        let mut meta = self.ensure_layout()?;
        meta.set(name, value);
        meta.set(okm_core::meta::VERSION, vec![self.schema.layout_version]);
        okm_core::meta::write(&self.store, &self.ns, &meta).map_err(codec)?;
        Ok(meta)
    }

    /// Declare an access method at runtime (binding-time registration,
    /// ADR-0022: the host language registers its callables after the
    /// table exists). The slot must be unique and non-zero — a collision
    /// is a caller bug surfaced as an error, not silently merged.
    pub fn declare_index(&mut self, index: AccessMethod) -> Result<(), String> {
        if index.slot == 0 || self.indexes.iter().any(|i| i.slot == index.slot) {
            return Err(format!(
                "access method slot {} must be unique and non-zero",
                index.slot
            ));
        }
        self.indexes.push(index);
        Ok(())
    }

    /// Declare a reduce group at runtime (binding-time registration).
    /// Slot rule mirrors `declare_index`.
    pub fn declare_reduce(&mut self, spec: crate::ReduceSpec) -> Result<(), String> {
        if spec.slot == 0 || self.reduces.iter().any(|r| r.spec.slot == spec.slot) {
            return Err(format!(
                "reduce slot {} must be unique and non-zero",
                spec.slot
            ));
        }
        self.reduces.push(crate::BoundReduce::new(spec));
        Ok(())
    }

    /// A reduce group's entry key for a group-field value map (read-side
    /// probe: the caller names the GROUP fields, same rule as the
    /// Rust-side `reduce_get` probe).
    pub fn reduce_entry_key(&self, _ns: u16, group_values: &ValueMap) -> Result<Vec<u8>, String> {
        // The first registered reduce owns this probe shape (single-reduce
        // tables; multi-reduce probes go through BoundReduce directly).
        let reduce = self
            .reduces
            .first()
            .ok_or_else(|| "no reduce registered".to_string())?;
        // Read probe: the caller's map supplies the group-field values;
        // it rides both the key and document slots so a group name from
        // EITHER source resolves (the values are the caller's).
        reduce.entry_key(&self.schema, &self.ns, group_values, group_values)
    }

    pub(crate) fn primary_key(&self, pkey: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3 + self.schema.key_len);
        buf.extend_from_slice(&self.ns);
        okm_core::write_slot(&mut buf, okm_core::index::PRIMARY_SLOT);
        buf.extend_from_slice(pkey);
        buf
    }

    /// The static segment takes declared names only — the row plan's
    /// invariant (`plan_put`). A map is the schema's stand-in for the struct
    /// here, so a name the schema does not declare is a caller bug rather than
    /// a value silently dropped on the floor. The whole-document verbs split by
    /// declaration before they get here, so this fires only on a direct
    /// `plan_put`; dynamic names belong to `merge` / `replace`.
    pub(crate) fn check_declared(&self, document: &ValueMap) -> Result<(), String> {
        for name in document.keys() {
            if !self.is_declared(name) {
                return Err(format!(
                    "the static segment takes declared names: `{name}` is not a \
                     declared field of this table — merge / replace write the segment"
                ));
            }
        }
        Ok(())
    }

    /// The stored document at `pkey`, decoded (None = absent): the row half
    /// of `fetch`, and the state every write verb reads before planning.
    fn stored_document(&self, pkey: &[u8]) -> Result<Option<ValueMap>, String> {
        if pkey.len() != self.schema.key_len {
            return Err(format!(
                "key width mismatch: got {}, schema declares {}",
                pkey.len(),
                self.schema.key_len
            ));
        }
        let Some(payload) = self
            .store
            .get(&self.primary_key(pkey))
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        let mut values = decode_payload(&self.schema, &payload).map_err(codec)?;
        crate::decode::resolve_vocab_labels(&self.schema, &self.store, &self.ns, &mut values)?;
        Ok(Some(values))
    }

    /// Replay a plan's ops against the local engine as ONE commit (the
    /// embedded execution arm). A native batch backend (fjall, redb, slatedb)
    /// executes the list as a single WAL unit; the trait's default replays the
    /// ops in order inside one call. Either way the document write, its faces,
    /// the reduce ledger and the dynamic-segment entry land together — the same
    /// one-commit rule the typed `Collection` keeps (ADR-0035 rule 6).
    fn replay(&self, ops: &[(Vec<u8>, Option<Vec<u8>>)]) -> Result<(), String> {
        if ops.is_empty() {
            return Ok(());
        }
        let mut batch = okm_core::Batch::default();
        for (key, value) in ops {
            match value {
                Some(v) => batch.put(key.clone(), v.clone()),
                None => batch.del(key),
            }
        }
        // The engine's refusal travels out as the write verb's `Err` — the same
        // `String` channel every other dynamic verb answers on, carrying the
        // engine's own text rather than a generic message.
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic write failed: {e}"))
    }

    /// Access-method scan (leftmost prefix over the indexed fields,
    /// caller-encoded): returns the matching documents' primary keys, decoded.
    /// THE routing primitive — an event resolves its targets here.
    pub fn scan(&self, index_slot: u16, encoded_prefix: &[u8]) -> Result<Vec<ValueMap>, String> {
        let index = self
            .indexes
            .iter()
            .find(|i| i.slot == index_slot)
            .ok_or_else(|| format!("no access method with slot {index_slot}"))?;
        scan_access_method(&self.store, &self.schema, &self.ns, index, encoded_prefix)
    }

    /// Every document's KEY (the pkey bytes), in key order: the rows (slot 0)
    /// merged with the segments that have NO row (slot 1) — the dynamic face's
    /// twin of `okm_core::Collection::scan_document_keys`.
    ///
    /// A document is the pair of slots (ADR-0035), so a `merge` / `replace` that
    /// carried no declared name left a document a primary scan cannot see. Two
    /// prefix scans plus an in-memory merge, no point read; materializing is
    /// [`fetch`](Self::fetch)'s job.
    pub fn scan_document_keys(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut rows = self.ns.clone();
        okm_core::write_slot(&mut rows, okm_core::index::PRIMARY_SLOT);
        let mut segs = self.ns.clone();
        okm_core::write_slot(&mut segs, okm_core::index::DYNAMIC_SLOT);
        let rows = self.store.scan_suffix(&rows).map_err(|e| e.to_string())?;
        let segs = self.store.scan_suffix(&segs).map_err(|e| e.to_string())?;
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
                out.push(rows[i].clone());
                i += 1;
            } else {
                out.push(segs[j].clone());
                j += 1;
            }
        }
        Ok(out)
    }

    /// Equality scan over one access method (same routing primitive as
    /// [`scan`](Self::scan)): only documents whose field segment is exactly
    /// the probe — the probe must name every field of the data segment.
    pub fn scan_exact(
        &self,
        index_slot: u16,
        encoded_prefix: &[u8],
    ) -> Result<Vec<ValueMap>, String> {
        let index = self
            .indexes
            .iter()
            .find(|i| i.slot == index_slot)
            .ok_or_else(|| format!("no access method with slot {index_slot}"))?;
        crate::index::scan_access_method_exact(
            &self.store,
            &self.schema,
            &self.ns,
            index,
            encoded_prefix,
        )
    }

    /// Covered scan over one access method: same range as [`scan`](Self::scan),
    /// but the entries' VALUE (the declared `includes` fields) is read and
    /// decoded, so the answer needs no document fetch. Returns
    /// `(key fields, covered fields)` per hit, both name-keyed.
    ///
    /// The read half of `includes`: `scan` returns keys and callers `fetch` each
    /// hit; this pays one entry-value decode instead. An access method declaring
    /// no `includes` covers nothing — the second map is empty.
    pub fn scan_covered(
        &self,
        index_slot: u16,
        encoded_prefix: &[u8],
    ) -> Result<Vec<(ValueMap, ValueMap)>, String> {
        let index = self
            .indexes
            .iter()
            .find(|i| i.slot == index_slot)
            .ok_or_else(|| format!("no access method with slot {index_slot}"))?;
        crate::index::scan_access_method_covered(
            &self.store,
            &self.schema,
            &self.ns,
            index,
            encoded_prefix,
        )
    }
    // --- Dynamic segment bridge (slot 1, vocabulary slots 2/3) -----------
    // The same per-document extension the typed Collection exposes:
    // name-keyed fields outside the schema's declared vocabulary, encoded
    // as nTLV frames in the dynamic entry, names resolved through the
    // per-table field-name vocabulary (one vocabulary per table; the
    // vocabulary lives inside the SAME ns segment as the data it names).
    // Dynamic-segment ops ride the plan surface (plan_fields): the
    // embedded path loads a vocabulary mirror from its engine, plans
    // WITHOUT engine access, and replays the ops — byte-identical to
    // what a remote caller ships as one wire frame (the same
    // plan+replay discipline as put/delete).
    //
    // The Value <-> DynamicValue bridge maps composites natively: the
    // dynamic segment is the schema-free zone, so nested Obj/Array are
    // legitimate field values here (schema-declared fixed-width paths
    // reject them upstream).

    /// Dynamic-segment entry key: `[ns 2B][DYNAMIC_SLOT 2B][pkey]` —
    /// identical byte layout to the typed path's `fields_key`.
    fn fields_key(&self, pkey: &[u8]) -> Vec<u8> {
        crate::plan_fields::fields_key(&self.ns, pkey)
    }

    /// The stored dynamic segment as a name-keyed map (`None` = no entry; a
    /// zero-frame list is never written, so empty and absent are one state).
    /// The read half every whole-document verb shares: `fetch` merges it with
    /// the row, `merge` / `replace` read-modify-write it.
    fn dynamic_map(&self, pkey: &[u8]) -> Result<Option<ValueMap>, String> {
        let raw = match self
            .store
            .get(&self.fields_key(pkey))
            .map_err(|e| e.to_string())?
        {
            Some(r) => r,
            None => return Ok(None),
        };
        // Fresh load per call — the embedded mirror of the old per-call
        // VocabCache (single writer: a reload always sees the truth it wrote).
        let vocab = crate::plan_fields::VocabMirror::load(&self.store, &self.ns)
            .map_err(|e| e.to_string())?;
        Ok(Some(crate::plan_fields::fields_from_frames(&raw, &vocab)?))
    }

    /// The document as ONE name-keyed map — the read door of the whole-document
    /// surface (ADR-0035), paired with `merge` / `replace` / `purge`. `None` =
    /// nothing in either slot: a dynamic-only document still fetches, because
    /// the document is the pair of slots, not the primary entry.
    ///
    /// A dynamic name that collides with a declared one shadows it, the rule
    /// the typed `fetch` applies (its `extend` order). `merge` and `replace`
    /// split by declaration and therefore cannot produce such a name; the only
    /// source is a write that predates this surface.
    pub fn fetch(&self, pkey: &[u8]) -> Result<Option<ValueMap>, String> {
        let row = self.stored_document(pkey)?;
        let dynamic = self.dynamic_map(pkey)?;
        if row.is_none() && dynamic.is_none() {
            return Ok(None);
        }
        let mut out = row.unwrap_or_default();
        if let Some(fields) = dynamic {
            out.extend(fields);
        }
        Ok(Some(out))
    }

    /// Partial whole-document update, ONE commit (ADR-0035 rule 3).
    ///
    /// - A name the schema declares is a read-modify-write of the row: the
    ///   stored document is decoded, the named fields overwrite it, and the row
    ///   is rewritten through the semantic path — the previous values' faces are
    ///   removed and the reduce ledger unfolds then refolds. Declared fields the
    ///   map does not name keep their stored values; a fresh row takes the
    ///   schema's defaults.
    /// - Every other name overwrites that name in the dynamic segment and
    ///   leaves the other dynamic names alone.
    /// - A map with no declared name never writes the row — and never READS it
    ///   either: the schema answers which arm the plan takes, so the stored
    ///   state is read per arm, not per verb (a dynamic-only map pays one
    ///   segment read, a declared-only map one row read).
    pub fn merge(&self, pkey: &[u8], object: &ValueMap) -> Result<(), String> {
        let (declared, dynamic) = self.split_names(object);
        let old_row = if declared.is_empty() {
            None
        } else {
            self.stored_document(pkey)?
        };
        let old_dynamic = if dynamic.is_empty() {
            None
        } else {
            self.dynamic_map(pkey)?
        };
        let plan = {
            let store = &self.store;
            // The vocabulary is the segment arm's business: a patch with no
            // non-declared name allocates nothing, so the mirror (two prefix
            // scans + a cursor read) is not loaded at all.
            let mut vocab = if dynamic.is_empty() {
                crate::plan_fields::VocabMirror::default()
            } else {
                crate::plan_fields::VocabMirror::load(store, &self.ns).map_err(|e| e.to_string())?
            };
            self.plan_merge(
                pkey,
                &declared,
                &dynamic,
                crate::plan::Stored {
                    row: old_row.as_ref(),
                    dynamic: old_dynamic.as_ref(),
                },
                &|ek| store.get(ek).map_err(|e| e.to_string()),
                &mut vocab,
            )?
        };
        self.replay(&plan.ops)?;
        Ok(())
    }

    /// Whole-document replacement, ONE commit: the map IS the document
    /// (ADR-0035 rule 4).
    ///
    /// - Declared names rebuild the row; a declared field the map does not name
    ///   takes the schema's default (the payload is positional — a half row
    ///   cannot be encoded). The row's previous faces are removed.
    /// - A map with NO declared name leaves the static segment absent, deleting
    ///   the row if it existed; the result is a dynamic-only document.
    /// - The dynamic segment is exactly the map's non-declared names, so
    ///   `replace(pkey, &empty_map)` is `purge(pkey)`.
    ///
    /// Reads: the row always (it is either rebuilt — sweep its faces — or
    /// deleted, and only the stored row says which); the segment only when a
    /// path index has entries to sweep, because the entry is written wholesale
    /// from the map (an empty map deletes it).
    pub fn replace(&self, pkey: &[u8], object: &ValueMap) -> Result<(), String> {
        let old_row = self.stored_document(pkey)?;
        let old_dynamic = if self.has_path_index() {
            self.dynamic_map(pkey)?
        } else {
            None
        };
        let (declared, dynamic) = self.split_names(object);
        let plan = {
            let store = &self.store;
            // Same rule as `merge`: with no dynamic name to write and no stored
            // segment to name, nothing needs the vocabulary.
            let mut vocab = if dynamic.is_empty() && old_dynamic.is_none() {
                crate::plan_fields::VocabMirror::default()
            } else {
                crate::plan_fields::VocabMirror::load(store, &self.ns).map_err(|e| e.to_string())?
            };
            self.plan_replace(
                pkey,
                &declared,
                &dynamic,
                crate::plan::Stored {
                    row: old_row.as_ref(),
                    dynamic: old_dynamic.as_ref(),
                },
                &|ek| store.get(ek).map_err(|e| e.to_string()),
                &mut vocab,
            )?
        };
        self.replay(&plan.ops)?;
        Ok(())
    }

    /// Remove the whole document — both slots — in one commit. `false` = there
    /// was nothing to remove (mirrors the typed `purge`).
    pub fn purge(&self, pkey: &[u8]) -> Result<bool, String> {
        let old_row = self.stored_document(pkey)?;
        let old_dynamic = self.dynamic_map(pkey)?;
        if old_row.is_none() && old_dynamic.is_none() {
            return Ok(false);
        }
        let plan = {
            let store = &self.store;
            self.plan_purge(pkey, old_row.as_ref(), old_dynamic.as_ref(), &|ek| {
                store.get(ek).map_err(|e| e.to_string())
            })?
        };
        self.replay(&plan.ops)?;
        Ok(true)
    }

    /// Is any path index declared on this table? The gate that keeps the
    /// old-map read off the common (no path index) write path.
    fn has_path_index(&self) -> bool {
        self.indexes
            .iter()
            .any(|i| matches!(i.kind, crate::AccessMethodKind::Path { .. }))
    }

    /// Path-index ops for one dynamic map: entries when `write`, the same
    /// entry keys as deletions otherwise (the sweep arm). Empty when no
    /// path index is declared. `pub(crate)` because the whole-document plan
    /// arms (`plan_dynamic`, `plan_purge`) owe the same sweep.
    pub(crate) fn path_entry_ops(
        &self,
        pkey: &[u8],
        fields: &ValueMap,
        write: bool,
    ) -> Result<Vec<EngineOp>, String> {
        if !self.has_path_index() {
            return Ok(Vec::new());
        }
        let entries =
            crate::index::path_index_entries(&self.schema, &self.ns, &self.indexes, pkey, fields)?;
        Ok(entries
            .into_iter()
            .map(|(k, v)| (k, if write { Some(v) } else { None }))
            .collect())
    }
}
