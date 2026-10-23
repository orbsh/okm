//! The plan surface (ADR-0022 remote mode): the put/delete expansion as
//! a pure function over caller-held state — no engine access.
//!
//! Embedded mode's `DynamicCollection::put`/`purge` and the remote
//! mode's `plan_put`/`plan_delete` share ONE expansion: the embedded
//! path is plan + local engine replay, the remote path ships the plan's
//! ops as one wire frame (ADR-0010 single `commit_batch` — document
//! write + index entries + acc updates land atomically). Byte equality
//! between the modes is structural, not a test coincidence.
//!
//! The caller holds the remote state: the OLD document (actor cache or
//! a prior GET — the read the embedded path does against its engine is
//! the remote caller's responsibility) and the current accumulators
//! (the actor IS the authoritative acc holder — ADR-0022's
//! single-writer-per-group ownership rule; steady state costs zero
//! extra round trips, only restart recovery reads accs once).

use okm_core::storage::VirtualStorage;
use crate::index::index_entries;
use crate::{decode_payload, encode_payload, DynamicCollection, ValueMap};
use okm_core::schema::CollectionSchema;

/// One planned write: the engine-shaped op list (MemBatch's
/// `(key, Option<value>)` — None = delete) plus the new accumulator
/// values the caller must adopt into its local cache.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedPut {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// Group entry key → the accumulator bytes that entry now holds.
    /// The actor updates its local acc cache from this receipt; the
    /// next plan reads the same keys through its `acc_of` closure.
    pub new_accs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// One planned delete: index-entry removals + acc unfold + primary
/// delete (empty when the caller says no old document exists — the
/// embedded path's delete of a missing key is a no-op, mirrored here).
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedDelete {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    pub new_accs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// One planned whole-document write (ADR-0035's map verbs, dynamic arm): the
/// row ops, the dynamic-segment ops and the path-index sweep in ONE list, so a
/// remote frame and the embedded replay land byte-identical ops and one commit.
/// `new_vocab_entries` is the field-name vocabulary growth the dynamic arm
/// allocated — the caller adopts it into its mirror before the next plan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlannedWrite {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    pub new_accs: Vec<(Vec<u8>, Vec<u8>)>,
    pub new_vocab_entries: Vec<(String, u16)>,
}

/// Current-accumulator lookup for one group entry key. `None` = the
/// actor's cache has no entry for the group (first fold seeds it).
pub type AccOf<'a> = dyn Fn(&[u8]) -> Option<Vec<u8>> + 'a;

/// The plan's own acc overlay: within ONE plan, a later arm reads the
/// acc a prior arm wrote (overwrite = unfold-then-fold on the same
/// group; the fold must see the unfolded value, not the engine's stale
/// one). The caller's cache adopts the overlay at the end (`new_accs`).
struct AccOverlay<'a> {
    base: &'a AccOf<'a>,
    written: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<'a> AccOverlay<'a> {
    fn new(base: &'a AccOf<'a>) -> Self {
        Self { base, written: std::collections::HashMap::new() }
    }
    fn get(&self, ek: &[u8]) -> Option<Vec<u8>> {
        match self.written.get(ek) {
            Some(v) => v.clone(),
            None => (self.base)(ek),
        }
    }
    fn put(&mut self, ek: Vec<u8>, acc: Vec<u8>) {
        self.written.insert(ek, Some(acc));
    }
}

impl<S: VirtualStorage> DynamicCollection<S> {
    /// Plan one put's full op list WITHOUT touching an engine. `old`
    /// is the currently-stored document at `pkey` (None = fresh key);
    /// `acc_of` resolves a group's current accumulator from the
    /// caller's cache.
    pub fn plan_put(
        &self,
        pkey: &[u8],
        document: &ValueMap,
        old: Option<&ValueMap>,
        acc_of: &AccOf,
    ) -> Result<PlannedPut, String> {
        self.check_key(pkey)?;
        self.check_declared(document)?;
        let mut plan = PlannedPut { ops: Vec::new(), new_accs: Vec::new() };
        let mut overlay = AccOverlay::new(acc_of);
        // Overwrite: sweep the old document's index entries + unfold it
        // from every group (the embedded put's overwrite arm, verbatim).
        if let Some(old_row) = old {
            let old_entries =
                index_entries(self.schema(), &self.ns, &self.indexes, pkey, old_row)?;
            for (ek, _) in old_entries {
                plan.ops.push((ek, None));
            }
            self.plan_reduces(pkey, old_row, false, &mut plan, &mut overlay)?;
        }
        // New entries + primary write.
        let entries =
            index_entries(self.schema(), &self.ns, &self.indexes, pkey, document)?;
        let payload = encode_payload(self.schema(), document).map_err(|e| e.to_string())?;
        self.plan_reduces(pkey, document, true, &mut plan, &mut overlay)?;
        plan.ops.push((self.primary_key(pkey), Some(payload)));
        for (ek, ev) in entries {
            plan.ops.push((ek, Some(ev)));
        }
        Ok(plan)
    }

    /// Plan one delete's op list WITHOUT touching an engine. `old` is
    /// the stored document (None = nothing to remove: an empty plan,
    /// mirroring the embedded delete's no-op on a missing key).
    pub fn plan_delete(
        &self,
        pkey: &[u8],
        old: &ValueMap,
        acc_of: &AccOf,
    ) -> Result<PlannedDelete, String> {
        self.check_key(pkey)?;
        let mut plan = PlannedDelete { ops: Vec::new(), new_accs: Vec::new() };
        let mut overlay = AccOverlay::new(acc_of);
        let entries =
            index_entries(self.schema(), &self.ns, &self.indexes, pkey, old)?;
        for (ek, _) in entries {
            plan.ops.push((ek, None));
        }
        self.plan_reduces(pkey, old, false, &mut plan, &mut overlay)?;
        plan.ops.push((self.primary_key(pkey), None));
        Ok(plan)
    }

    /// Plan one partial whole-document update (ADR-0035's `merge`), WITHOUT
    /// touching an engine. `old_row` / `old_dynamic` are the two stored halves
    /// the caller holds (a fresh key = both `None`); `vocab` is the caller's
    /// field-name mirror — it allocates ids for first-seen dynamic names, and
    /// the receipt rides the plan.
    ///
    /// Declared names are a row read-modify-write through [`plan_put`]
    /// (Self::plan_put): faces swept, ledger unfolded then refolded. Every other
    /// name is a read-modify-write of the dynamic segment. A map with no
    /// declared name emits no row op at all.
    pub fn plan_merge(
        &self,
        pkey: &[u8],
        object: &ValueMap,
        old_row: Option<&ValueMap>,
        old_dynamic: Option<&ValueMap>,
        acc_of: &AccOf,
        vocab: &mut crate::plan_fields::VocabMirror,
    ) -> Result<PlannedWrite, String> {
        self.check_key(pkey)?;
        let (declared, dynamic) = self.split_names(object);
        let mut plan = PlannedWrite::default();
        if !declared.is_empty() {
            let mut carried: Vec<&ValueMap> = Vec::with_capacity(2);
            if let Some(old) = old_row {
                carried.push(old);
            }
            carried.push(&declared);
            let row_map = self.row_base(pkey, &carried)?;
            let row = self.plan_put(pkey, &row_map, old_row, acc_of)?;
            plan.ops.extend(row.ops);
            plan.new_accs = row.new_accs;
        }
        if !dynamic.is_empty() {
            let mut stored = old_dynamic.cloned().unwrap_or_default();
            stored.extend(dynamic);
            self.plan_dynamic(pkey, old_dynamic, &stored, vocab, &mut plan)?;
        }
        Ok(plan)
    }

    /// Plan one whole-document replacement (ADR-0035's `replace`): the map IS
    /// the document. Declared names rebuild the row (absent ones take the
    /// schema's default); a map with NO declared name deletes the row and
    /// leaves a dynamic-only document. The segment is exactly the map's
    /// non-declared names, so an empty map plans the purge.
    pub fn plan_replace(
        &self,
        pkey: &[u8],
        object: &ValueMap,
        old_row: Option<&ValueMap>,
        old_dynamic: Option<&ValueMap>,
        acc_of: &AccOf,
        vocab: &mut crate::plan_fields::VocabMirror,
    ) -> Result<PlannedWrite, String> {
        self.check_key(pkey)?;
        let (declared, dynamic) = self.split_names(object);
        let mut plan = PlannedWrite::default();
        if declared.is_empty() {
            if let Some(old) = old_row {
                let row = self.plan_delete(pkey, old, acc_of)?;
                plan.ops.extend(row.ops);
                plan.new_accs = row.new_accs;
            }
        } else {
            let row_map = self.row_base(pkey, &[&declared])?;
            let row = self.plan_put(pkey, &row_map, old_row, acc_of)?;
            plan.ops.extend(row.ops);
            plan.new_accs = row.new_accs;
        }
        self.plan_dynamic(pkey, old_dynamic, &dynamic, vocab, &mut plan)?;
        Ok(plan)
    }

    /// Plan one whole-document removal (ADR-0035's `purge`): the row arm when
    /// the caller holds a stored row, the dynamic entry plus its path-index
    /// sweep when it holds a stored segment. Nothing to remove plans no ops.
    pub fn plan_purge(
        &self,
        pkey: &[u8],
        old_row: Option<&ValueMap>,
        old_dynamic: Option<&ValueMap>,
        acc_of: &AccOf,
    ) -> Result<PlannedWrite, String> {
        self.check_key(pkey)?;
        let mut plan = PlannedWrite::default();
        if let Some(old) = old_row {
            let row = self.plan_delete(pkey, old, acc_of)?;
            plan.ops.extend(row.ops);
            plan.new_accs = row.new_accs;
        }
        if let Some(old) = old_dynamic {
            plan.ops.extend(self.path_entry_ops(pkey, old, false)?);
            plan.ops.push((crate::plan_fields::fields_key(&self.ns, pkey), None));
        }
        Ok(plan)
    }

    /// The dynamic-segment arm shared by `plan_merge` / `plan_replace`: the
    /// entry's ops (plus any vocabulary growth) and the path-index sweep.
    /// Order matters — the entry and its path entries are distinct keys, and a
    /// value present in both the old and the new map must end on its PUT, never
    /// on the delete that follows.
    fn plan_dynamic(
        &self,
        pkey: &[u8],
        old: Option<&ValueMap>,
        new: &ValueMap,
        vocab: &mut crate::plan_fields::VocabMirror,
        plan: &mut PlannedWrite,
    ) -> Result<(), String> {
        let fields = crate::plan_fields::plan_put_fields(&self.ns, pkey, new, vocab)?;
        plan.ops.extend(fields.ops);
        plan.new_vocab_entries = fields.new_vocab_entries;
        if let Some(old) = old {
            plan.ops.extend(self.path_entry_ops(pkey, old, false)?);
        }
        plan.ops.extend(self.path_entry_ops(pkey, new, true)?);
        Ok(())
    }

    /// Split a name-keyed map along the PAYLOAD declaration: a name the row carries
    /// (hot or cold) is the static segment, every other name the dynamic one.
    ///
    /// KEY fields are deliberately NOT part of the test. The key is the identity
    /// and it is a separate thing from the value — OKM is a KV store whose row
    /// is a name-keyed map, so a value may perfectly well carry a field that
    /// happens to share a key field's name. It is a value field, it lands in the
    /// dynamic segment, and the identity still comes from the pkey (which no map
    /// can move). Rejecting it would be a rule about a collision that cannot
    /// happen: the row half never carries key names, so nothing is shadowed.
    pub(crate) fn split_names(&self, object: &ValueMap) -> (ValueMap, ValueMap) {
        let mut declared = ValueMap::new();
        let mut dynamic = ValueMap::new();
        for (name, value) in object {
            if self.is_payload_field(name) {
                declared.insert(name.clone(), value.clone());
            } else {
                dynamic.insert(name.clone(), value.clone());
            }
        }
        (declared, dynamic)
    }

    /// Is this name part of the row's PAYLOAD? Key fields are not: they are the
    /// identity, and the identity has its own door (the `pkey`). This is the
    /// CLASSIFIER — the segment rule is about the value, not about the key.
    pub(crate) fn is_payload_field(&self, name: &str) -> bool {
        self.schema()
            .hot_fields
            .iter()
            .chain(self.schema().cold_fields.iter())
            .any(|f| f.name == name)
    }

    /// Is this name one the schema OWNS at all (identity or payload)? The row
    /// map an assembled write carries holds both — `row_base` folds the identity
    /// in from the pkey — so validation asks this, while classification asks
    /// [`Self::is_payload_field`].
    pub(crate) fn is_declared(&self, name: &str) -> bool {
        self.schema()
            .key_fields
            .iter()
            .chain(self.schema().hot_fields.iter())
            .chain(self.schema().cold_fields.iter())
            .any(|f| f.name == name)
    }

    /// The complete row a write starts from: the carried maps (stored row, then
    /// the caller's declared names), the identity decoded out of `pkey`, then
    /// the schema's defaults for whatever is still absent.
    ///
    /// The identity comes from the KEY, never from a map: a map that names a key
    /// field is writing a DYNAMIC field of that name (see `split_names`), not the
    /// identity — the pkey stays authoritative. `encode_payload` requires every declared
    /// field, and "absent" means the default on the decode side too
    /// ([`crate::decode::fill_schema_defaults`], one definition).
    fn row_base(&self, pkey: &[u8], carried: &[&ValueMap]) -> Result<ValueMap, String> {
        let mut out = ValueMap::new();
        for map in carried {
            out.extend((*map).clone());
        }
        out.extend(crate::decode_key(self.schema(), pkey).map_err(|e| e.to_string())?);
        crate::decode::fill_schema_defaults(self.schema(), &mut out);
        Ok(out)
    }

    /// The reduce calling discipline, planned form: same order and the
    /// same seed/unfold-missing rules as the embedded `apply_reduces`,
    /// reading accs through `acc_of` instead of the engine.
    fn plan_reduces(
        &self,
        pkey: &[u8],
        document: &ValueMap,
        add: bool,
        plan: &mut impl PlanSink,
        overlay: &mut AccOverlay,
    ) -> Result<(), String> {
        // The key decoded ONCE per plan (schema-driven slicing of the
        // caller's pkey — the embedded path's &Key twin).
        let key = crate::decode_key(self.schema(), pkey).map_err(|e| e.to_string())?;
        for reduce in &self.reduces {
            let ek = reduce.entry_key(self.schema(), &self.ns, &key, document)?;
            let mut acc = match overlay.get(&ek) {
                Some(bytes) => bytes,
                // Seed only on the fold arm (same rule as embedded):
                // an unfold hitting a missing entry is a discipline
                // violation, not a zero group.
                None if add => reduce.spec.logic.seed(),
                None => return Ok(()),
            };
            if add {
                reduce.spec.logic.fold(&mut acc, &key, document)?;
            } else {
                reduce.spec.logic.unfold(&mut acc, &key, document)?;
            }
            plan.push((ek.clone(), Some(acc.clone())));
            plan.push_acc((ek.clone(), acc.clone()));
            overlay.put(ek, acc);
        }
        Ok(())
    }

    fn check_key(&self, pkey: &[u8]) -> Result<(), String> {
        if pkey.len() != self.schema().key_len {
            return Err(format!(
                "key width mismatch: got {}, schema declares {}",
                pkey.len(),
                self.schema().key_len
            ));
        }
        Ok(())
    }
}

/// The two plan shapes share the sink's push form.
trait PlanSink {
    fn push(&mut self, op: (Vec<u8>, Option<Vec<u8>>));
    fn push_acc(&mut self, entry: (Vec<u8>, Vec<u8>));
}

impl PlanSink for PlannedPut {
    fn push(&mut self, op: (Vec<u8>, Option<Vec<u8>>)) {
        self.ops.push(op);
    }
    fn push_acc(&mut self, entry: (Vec<u8>, Vec<u8>)) {
        self.new_accs.push(entry);
    }
}

impl PlanSink for PlannedDelete {
    fn push(&mut self, op: (Vec<u8>, Option<Vec<u8>>)) {
        self.ops.push(op);
    }
    fn push_acc(&mut self, entry: (Vec<u8>, Vec<u8>)) {
        self.new_accs.push(entry);
    }
}

/// Decode a stored payload into the caller-side `old` document (the
/// remote actor's cache-refill helper — what the embedded path does
/// against its engine on overwrite, offered here so the binding wraps
/// it instead of re-implementing the decode).
pub fn decode_stored(schema: &CollectionSchema, payload: &[u8]) -> Result<ValueMap, String> {
    decode_payload(schema, payload).map_err(|e| e.to_string())
}
