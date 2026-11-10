//! Replay: a log directory → a store built by the current build (ADR-0056 §5–§6).
//!
//! Replay is the migration consumer, and it is not a copy. A copy would carry the
//! old table's bytes into the new store, which is exactly wrong when the reason for
//! the move is a different layout, a different vocabulary allocation, or a
//! different set of declared faces: the new build must WRITE the rows through its
//! own write path, so index entries, dynamic segments and reduce ledgers are
//! rebuilt from the rows instead of inherited (ADR-0056 §5).
//!
//! Three rules follow, and each one is a decision rather than an implementation
//! detail:
//!
//! - **Rows, not bytes.** A captured put is decoded with the source declaration and
//!   re-written by the target's write path; the payload's vocabulary ids are
//!   re-resolved `id → label → new id`, because the target allocates its own ids in
//!   its own order.
//! - **Order is the snapshot first, then the changes above it.** A baseline is
//!   written after every delta it covers, so a replay applies the newest baseline
//!   and then every delta numbered above it — the deltas below are already inside
//!   the snapshot (ADR-0056 §3).
//! - **A removal is applied as a removal.** The primary face's delete means the row
//!   is gone; the other faces' deletes are the document's own parts and are
//!   answered by the write path's purge, not replayed one by one.
//!
//! What replay cannot repair is a change the log never carried (a crash between the
//! commit and the append). That is what [`reconcile`] is for (ADR-0056 §4): once the
//! old instance has stopped writing, the OLD STORE — not the log — is the record, so
//! one range scan of its faces makes the target agree with it. It is the last step of
//! a migration and the only one that can repair a delete the log lost.
//!
//! **Where this lives, and why.** The mechanism is core's: which segments to apply in
//! which order, how a captured row is decoded and re-interned, what a removal means.
//! What core cannot name is the TARGET — the schema-driven collection lives in
//! `okm-dynamic`, a crate above core — so the seam is a trait here ([`ReplayTarget`])
//! and the implementation sits with the type that implements it.
//!
//! The sequence, because the order is what makes it safe (ADR-0056 §6): the new build
//! runs as a second process and replays what is already sealed WHILE the old instance
//! keeps serving; the old instance is then stopped gracefully, and that stop is itself
//! a write — `Drop for Changelog` flushes the pending frames and seals the tail into
//! one last segment (a crash leaves a `.partial` that readers ignore); the new build
//! applies that tail, and [`reconcile`] runs before cutover. So no step here needs the
//! old instance stopped FIRST — only the last two do.

use super::{baseline_segments, read_sealed, sealed_segments};
use crate::VirtualStorage;
use crate::decode::{decode_key, decode_payload};
use crate::model::index::DYNAMIC_SLOT;
use crate::model::obj_dynamic::{DynamicValue, ValueMap};
use crate::{OP_DELETE, OP_PUT, PRIMARY_SLOT};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What a replay needs from the thing it replays INTO (ADR-0056 §5).
///
/// The trait exists because the mechanism and the target belong on different sides of
/// the dependency line: the log is core's, the schema-driven collection is
/// `okm-dynamic`'s (which depends on core, so core cannot name it). Core owns the
/// seam; the crate above implements it once, by delegation.
pub trait ReplayTarget {
    type Store: VirtualStorage;
    /// The store behind the target — replay reads the target's OWN vocabulary while it
    /// interns labels into that target's ids.
    fn store(&self) -> &Self::Store;
    /// The table's name, for messages.
    fn name(&self) -> &str;
    /// Write a decoded row through the target's own write path (never a byte copy:
    /// index entries, dynamic segments and reduce ledgers are rebuilt from the row).
    fn merge(&self, pkey: &[u8], row: &ValueMap) -> Result<(), String>;
    /// Remove a row. A primary-face delete is a removal.
    fn purge(&self, pkey: &[u8]) -> Result<bool, String>;
    /// The row as the target would read it back — labels, not ids. Reconciliation
    /// compares in labels, because that is what a read hands out.
    fn fetch(&self, pkey: &[u8]) -> Result<Option<ValueMap>, String>;
    /// Every document key the target holds: the subtraction half of a reconciliation.
    fn document_keys(&self) -> Result<Vec<Vec<u8>>, String>;
}

/// What one replay did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplayReport {
    /// Segments applied, in order (baseline included when there was one).
    pub segments: usize,
    pub puts: usize,
    pub deletes: usize,
    /// Dynamic segments carried over (a document's non-declared fields). Counted
    /// apart from `puts` because a put is a row and this is a segment beside it.
    pub dynamic: usize,
    /// Ops with no column and no segment of their own (a face the write path
    /// rebuilds from the row: an index entry, a reduce ledger line).
    pub rederived: usize,
}

/// Replay one table directory into `target`, through its write path.
///
/// `target` is built from the SAME declaration the log was written under (the
/// sidecar beside the segments, or a schema the new build exported). Tables whose
/// declaration changed shape are the caller's to translate before this point: a
/// replay does not guess field renames.
pub fn replay_table<T: ReplayTarget>(dir: &Path, target: &T) -> Result<ReplayReport, String> {
    let schema = super::window::load_sidecar(dir).map_err(|e| e.to_string())?;
    let header = super::window::header_of(dir).map_err(|e| e.to_string())?;

    let mut report = ReplayReport::default();
    let mut labels: HashMap<(u16, Vec<u8>), String> = HashMap::new();

    for segment in ordered_segments(dir)? {
        report.segments += 1;
        for frame in read_sealed(&segment).map_err(|e| e.to_string())? {
            for (tag, key, value) in frame.0 {
                let Some((slot, suffix)) = super::window::split_slot(&header, &key) else {
                    continue;
                };
                match (tag, slot) {
                    (OP_PUT, PRIMARY_SLOT) if suffix.len() == schema.key_len => {
                        let row =
                            row_of(&schema, &header, target.store(), suffix, &value, &labels)?;
                        target
                            .merge(suffix, &row)
                            .map_err(|e| format!("{}: {e}", dir.display()))?;
                        report.puts += 1;
                    }
                    // The dynamic segment is truth too (ADR-0045 §4): names the
                    // declaration does not have, in the same nTLV frames. It is NOT
                    // re-derived from the row — a row write only rebuilds declared
                    // fields — so replay has to carry it, or every document with a
                    // dynamic field would arrive without one.
                    (OP_PUT, DYNAMIC_SLOT) => {
                        let dynamic = super::window::decode_dynamic_segment(&value, &labels)
                            .map_err(|e| e.to_string())?;
                        if !dynamic.is_empty() {
                            target
                                .merge(suffix, &dynamic)
                                .map_err(|e| format!("{}: {e}", dir.display()))?;
                            report.dynamic += 1;
                        }
                    }
                    (OP_PUT, slot) if super::window::is_id_face(&schema, slot) => {
                        if let Ok(name) = String::from_utf8(value) {
                            labels.insert((slot, suffix.to_vec()), name);
                        }
                        // Not the target's ids: it allocates its own.
                        report.rederived += 1;
                    }
                    (OP_PUT, _) => report.rederived += 1,
                    (OP_DELETE, PRIMARY_SLOT) => {
                        target
                            .purge(suffix)
                            .map_err(|e| format!("{}: {e}", dir.display()))?;
                        report.deletes += 1;
                    }
                    // A part of a document (an index face, a dynamic entry): the
                    // write path rewrites those from the row.
                    (OP_DELETE, _) => report.rederived += 1,
                    _ => {}
                }
            }
        }
    }
    Ok(report)
}

/// One table's outcome: what the replay applied, and — when an old store was
/// given — what the reconciliation had to repair afterwards.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableRun {
    /// The table's header (its ns) in hex — the name it has on disk.
    pub table: String,
    pub replay: ReplayReport,
    pub reconcile: Option<ReconcileReport>,
}

/// Replay every table directory under a log root.
pub fn replay_all<T, F>(root: &Path, mut target: F) -> Result<Vec<TableRun>, String>
where
    T: ReplayTarget,
    F: FnMut(u16, &crate::schema::CollectionSchema) -> T,
{
    per_table(root, |dir, header, schema| {
        let collection = target(ns_of(header, dir)?, schema);
        Ok(TableRun {
            table: super::window::file_name(dir),
            replay: replay_table(dir, &collection)?,
            reconcile: None,
        })
    })
}

/// Replay every table, and — when `old` is the store the log came from — make the
/// target agree with it afterwards (ADR-0056 §4).
///
/// The two halves belong behind one call because their ORDER is the whole point:
/// replay is fast and incomplete (write-after loses appends), the scan is slow and
/// complete (the store IS the record), and a migration that skips the second calls
/// the first one the answer.
pub fn replay_and_reconcile<T, F, O>(
    root: &Path,
    mut target: F,
    old: &O,
) -> Result<Vec<TableRun>, String>
where
    T: ReplayTarget,
    F: FnMut(u16, &crate::schema::CollectionSchema) -> T,
    O: VirtualStorage + ?Sized,
{
    per_table(root, |dir, header, schema| {
        let collection = target(ns_of(header, dir)?, schema);
        Ok(TableRun {
            table: super::window::file_name(dir),
            replay: replay_table(dir, &collection)?,
            reconcile: Some(reconcile(old, &collection, header, schema)?),
        })
    })
}

/// The one directory walk both entry points share: a table directory, its header,
/// its declaration.
fn per_table<F>(root: &Path, mut run: F) -> Result<Vec<TableRun>, String>
where
    F: FnMut(&Path, &[u8], &crate::schema::CollectionSchema) -> Result<TableRun, String>,
{
    let mut out = Vec::new();
    for dir in super::window::table_dirs(root).map_err(|e| e.to_string())? {
        let schema = super::window::load_sidecar(&dir).map_err(|e| e.to_string())?;
        let header = super::window::header_of(&dir).map_err(|e| e.to_string())?;
        out.push(run(&dir, &header, &schema)?);
    }
    Ok(out)
}

/// A table directory's header IS one ns (two bytes); anything else is not a table.
fn ns_of(header: &[u8], dir: &Path) -> Result<u16, String> {
    Ok(u16::from_be_bytes(header.try_into().map_err(|_| {
        format!("{}: header is not one ns", dir.display())
    })?))
}

/// The segments to apply, in the one order that is correct: the newest baseline
/// first, then every delta numbered above it.
fn ordered_segments(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let header = super::window::header_of(dir).map_err(|e| e.to_string())?;
    // The listing helpers take the log ROOT and derive the table directory from the
    // header; a table directory is `<root>/<header hex>`.
    let root = dir
        .parent()
        .ok_or_else(|| format!("{}: a table directory has a parent", dir.display()))?;
    let baselines = baseline_segments(root, &header).map_err(|e| e.to_string())?;
    let deltas = sealed_segments(root, &header).map_err(|e| e.to_string())?;

    let mut ordered = Vec::new();
    let mut floor = String::new();
    if let Some(base) = baselines.last() {
        floor = super::window::file_name(base);
        ordered.push(base.clone());
    }
    // A delta numbered below the snapshot is inside it: applying it again would
    // resurrect a row the snapshot no longer has.
    ordered.extend(
        deltas
            .into_iter()
            .filter(|p| super::window::file_name(p) > floor),
    );
    Ok(ordered)
}

/// Decode one captured row into the document the target's write path expects.
///
/// Two id translations, in this order: the log's label frames name the ids the
/// capture wrote (`id → label`), then the target's vocabulary names them again in
/// its own numbering (`label → new id`). An id the log carried no label for stays
/// bytes — a reader never invents a name (ADR-0032).
fn row_of<S: crate::VirtualStorage>(
    schema: &crate::schema::CollectionSchema,
    header: &[u8],
    target_store: &S,
    suffix: &[u8],
    payload: &[u8],
    labels: &HashMap<(u16, Vec<u8>), String>,
) -> Result<ValueMap, String> {
    let mut row = decode_payload(schema, payload).map_err(|e| e.to_string())?;
    let _key = decode_key(schema, suffix).map_err(|e| e.to_string())?;
    for field in schema.hot_fields.iter().chain(schema.cold_fields.iter()) {
        let crate::FieldType::Vocab(face) = field.ty else {
            continue;
        };
        let Some(DynamicValue::Bytes(id)) = row.get(&field.name).cloned() else {
            continue;
        };
        if let Some(label) = labels.get(&(face, id)) {
            row.insert(field.name.clone(), DynamicValue::Str(label.clone()));
        }
    }
    intern(schema, header, target_store, &mut row)?;
    Ok(row)
}

/// `label → the target's own id`, written back as bytes. The target allocates in
/// its own order, so an id copied from elsewhere would point at the wrong label.
fn intern<S: crate::VirtualStorage>(
    schema: &crate::schema::CollectionSchema,
    header: &[u8],
    store: &S,
    row: &mut ValueMap,
) -> Result<(), String> {
    for field in schema.hot_fields.iter().chain(schema.cold_fields.iter()) {
        let crate::FieldType::Vocab(face) = field.ty else {
            continue;
        };
        let Some(DynamicValue::Str(label)) = row.get(&field.name).cloned() else {
            continue;
        };
        let faces = crate::obj_vocab::VocabFaces::from_id_slot(face);
        let Some(id) = crate::obj_vocab::vocab_id(store, header, faces, field.width, &label)
            .map_err(|e| e.to_string())?
        else {
            // The declared width is not one a vocabulary uses: the schema and the
            // store disagree, which is a stop rather than a guess.
            return Err(format!(
                "field `{}`: vocabulary width {} is not 1/2/4/8/16",
                field.name, field.width
            ));
        };
        // The id as BYTES: `Vocab<N>` is a width-typed byte id, which is what the
        // encoder takes and what a decode hands back (ADR-0032).
        row.insert(field.name.clone(), DynamicValue::Bytes(id));
    }
    Ok(())
}

/// What the reconciliation pass changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReconcileReport {
    /// Rows the old store had and the target did not: writes the log never carried.
    pub adopted: usize,
    /// Rows the target had and the old store did not: removals the log never carried.
    pub removed: usize,
    /// Rows both held with the same values.
    pub unchanged: usize,
}

/// Make `target` agree with `old` for one table (ADR-0056 §4).
///
/// The whole point is that this pass trusts the STORE, not the log: the log is
/// write-after, so a crash between a commit and its append loses that change, and a
/// lost append of a delete is the one thing a replay can never notice on its own (no
/// segment mentions the row at all). One scan of the old store's primary face is the
/// complete answer for the truth half — every row that exists now, and by subtraction
/// every row that does not.
///
/// Run it AFTER the old instance has stopped writing: between the scan and the last
/// write there is no way to tell a migration in progress from a divergence.
///
/// The two stores must hold the same shape. A layout mismatch is refused rather than
/// half-decoded: reconciling across shapes needs the declared translation, and that
/// is a deliberate act, not a flag.
pub fn reconcile<O, T: ReplayTarget>(
    old: &O,
    target: &T,
    header: &[u8],
    schema: &crate::schema::CollectionSchema,
) -> Result<ReconcileReport, String>
where
    O: VirtualStorage + ?Sized,
{
    use crate::engine::storage::scan_suffix_kv;

    // Same shape, or stop: every reference below decodes with THIS declaration.
    let old_meta = crate::meta::read(old, header).map_err(|e| e.to_string())?;
    if let Some(meta) = &old_meta
        && let Some(version) = meta.version()
        && version != schema.layout_version
    {
        return Err(format!(
            "the old store is layout {version}, this declaration is {} — a reconciliation \
             across shapes needs an explicit translation, not a scan",
            schema.layout_version
        ));
    }

    let mut prefix = header.to_vec();
    crate::write_slot(&mut prefix, PRIMARY_SLOT);
    let face = scan_suffix_kv(old, &prefix).map_err(|e| e.to_string())?;

    // A document can exist with NO primary row — a dynamic-only document is a
    // segment beside a key (`merge` of nothing but non-declared names). The
    // subtraction half below would purge it as a lost delete unless the dynamic
    // face counts as presence too.
    let mut dynamic_prefix = header.to_vec();
    crate::write_slot(&mut dynamic_prefix, DYNAMIC_SLOT);
    let dynamic_face = scan_suffix_kv(old, &dynamic_prefix).map_err(|e| e.to_string())?;

    let mut report = ReconcileReport::default();
    let mut present: HashMap<Vec<u8>, ()> = HashMap::with_capacity(face.len() + dynamic_face.len());
    present.extend(dynamic_face.into_iter().map(|(pkey, _)| (pkey, ())));
    for (pkey, payload) in face {
        present.insert(pkey.clone(), ());
        let mut row = decode_payload(schema, &payload).map_err(|e| e.to_string())?;
        // The old store's own vocabulary names the ids in its own rows — the one
        // place they are still resolvable after the log's label frames are gone.
        for field in schema.hot_fields.iter().chain(schema.cold_fields.iter()) {
            let crate::FieldType::Vocab(id_slot) = field.ty else {
                continue;
            };
            let Some(DynamicValue::Bytes(raw)) = row.get(&field.name).cloned() else {
                continue;
            };
            let faces = crate::obj_vocab::VocabFaces::from_id_slot(id_slot);
            if let Some(label) = crate::obj_vocab::vocab_label(old, header, faces, &raw)
                .map_err(|e| e.to_string())?
            {
                row.insert(field.name.clone(), DynamicValue::Str(label));
            }
        }

        // Compared in LABELS, because that is what a read hands back (a fetch
        // resolves ids); the id form is only what the write path wants. Comparing
        // the raw id bytes against a resolved row would call every row changed and
        // rewrite the whole table to reach the state it was already in.
        match target.fetch(&pkey).map_err(|e| e.to_string())? {
            Some(have) if have == row => report.unchanged += 1,
            _ => {
                intern(schema, header, target.store(), &mut row)?;
                target
                    .merge(&pkey, &row)
                    .map_err(|e| format!("{}: {e}", target.name()))?;
                report.adopted += 1;
            }
        }
    }

    // The subtraction half: the target holds rows the store no longer has. That is a
    // delete whose append was lost, and it is invisible to the log.
    for pkey in target.document_keys().map_err(|e| e.to_string())? {
        if present.contains_key(&pkey) {
            continue;
        }
        target
            .purge(&pkey)
            .map_err(|e| format!("{}: {e}", target.name()))?;
        report.removed += 1;
    }
    Ok(report)
}
