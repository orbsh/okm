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
//! commit and the append). That is what the post-migration reconciliation scan is
//! for (ADR-0056 §4), and it is the operator's step, not this module's.

use okm_core::changelog::{baseline_segments, read_sealed, sealed_segments};
use okm_core::{OP_DELETE, OP_PUT, PRIMARY_SLOT};
use okm_dynamic::{DynamicCollection, Value, ValueMap, decode_key, decode_payload};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What one replay did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplayReport {
    /// Segments applied, in order (baseline included when there was one).
    pub segments: usize,
    pub puts: usize,
    pub deletes: usize,
    /// Dynamic-segment ops with no column in the declaration; the write path
    /// rebuilds the dynamic segment from the document, so these are re-derived
    /// rather than copied.
    pub rederived: usize,
}

/// Replay one table directory into `target`, through its write path.
///
/// `target` is built from the SAME declaration the log was written under (the
/// sidecar beside the segments, or a schema the new build exported). Tables whose
/// declaration changed shape are the caller's to translate before this point: a
/// replay does not guess field renames.
pub fn replay_table<S: okm_core::VirtualStorage>(
    dir: &Path,
    target: &DynamicCollection<S>,
) -> Result<ReplayReport, String> {
    let schema = okm_core::changelog::window::load_sidecar(dir).map_err(|e| e.to_string())?;
    let header = okm_core::changelog::window::header_of(dir).map_err(|e| e.to_string())?;

    let mut report = ReplayReport::default();
    let mut labels: HashMap<(u16, Vec<u8>), String> = HashMap::new();

    for segment in ordered_segments(dir)? {
        report.segments += 1;
        for frame in read_sealed(&segment).map_err(|e| e.to_string())? {
            for (tag, key, value) in frame.0 {
                let Some((slot, suffix)) = okm_core::changelog::window::split_slot(&header, &key)
                else {
                    continue;
                };
                match (tag, slot) {
                    (OP_PUT, PRIMARY_SLOT) if suffix.len() == schema.key_len => {
                        let row = row_of(
                            &schema,
                            &header,
                            target.store(),
                            suffix,
                            &value,
                            &mut labels,
                        )?;
                        target
                            .merge(suffix, &row)
                            .map_err(|e| format!("{}: {e}", dir.display()))?;
                        report.puts += 1;
                    }
                    (OP_PUT, slot) if is_id_face(&schema, slot) => {
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

/// Replay every table directory under a log root.
pub fn replay_all<S, F>(root: &Path, mut target: F) -> Result<Vec<ReplayReport>, String>
where
    S: okm_core::VirtualStorage,
    F: FnMut(u16, &okm_core::schema::CollectionSchema) -> DynamicCollection<S>,
{
    let mut out = Vec::new();
    for dir in okm_core::changelog::window::table_dirs(root).map_err(|e| e.to_string())? {
        let schema = okm_core::changelog::window::load_sidecar(&dir).map_err(|e| e.to_string())?;
        let ns = u16::from_be_bytes(
            okm_core::changelog::window::header_of(&dir)
                .map_err(|e| e.to_string())?
                .as_slice()
                .try_into()
                .map_err(|_| format!("{}: header is not one ns", dir.display()))?,
        );
        let collection = target(ns, &schema);
        out.push(replay_table(&dir, &collection)?);
    }
    Ok(out)
}

/// The segments to apply, in the one order that is correct: the newest baseline
/// first, then every delta numbered above it.
fn ordered_segments(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let header = okm_core::changelog::window::header_of(dir).map_err(|e| e.to_string())?;
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
        floor = okm_core::changelog::window::file_name(base);
        ordered.push(base.clone());
    }
    // A delta numbered below the snapshot is inside it: applying it again would
    // resurrect a row the snapshot no longer has.
    ordered.extend(
        deltas
            .into_iter()
            .filter(|p| okm_core::changelog::window::file_name(p) > floor),
    );
    Ok(ordered)
}

/// Decode one captured row into the document the target's write path expects, with
/// every vocabulary id replaced by its label.
fn row_of<S: okm_core::VirtualStorage>(
    schema: &okm_core::schema::CollectionSchema,
    header: &[u8],
    store: &S,
    suffix: &[u8],
    payload: &[u8],
    labels: &mut HashMap<(u16, Vec<u8>), String>,
) -> Result<ValueMap, String> {
    let mut row = decode_payload(schema, payload).map_err(|e| e.to_string())?;
    let _key = decode_key(schema, suffix).map_err(|e| e.to_string())?;
    for field in schema.hot_fields.iter().chain(schema.cold_fields.iter()) {
        let okm_core::FieldType::Vocab(face) = field.ty else {
            continue;
        };
        let Some(Value::Bytes(id)) = row.get(&field.name).cloned() else {
            continue;
        };
        // `id → label → new id`, made explicit: the label the log carried is
        // interned in the TARGET's vocabulary — which allocates there, in the
        // target's own order — and the row carries that number. An id whose label
        // the log never carried stays bytes: a reader never invents a name
        // (ADR-0032).
        let Some(label) = labels.get(&(face, id)) else {
            continue;
        };
        let faces = okm_core::obj_vocab::VocabFaces::from_id_slot(face);
        let Some(fresh) =
            okm_core::obj_vocab::vocab_id(store, header, faces, field.width, label)
                .map_err(|e| e.to_string())?
        else {
            // The declared width is not one a vocabulary uses: the schema and the
            // store disagree, which is a stop rather than a guess.
            return Err(format!(
                "field `{}`: vocabulary width {} is not 1/2/4/8/16",
                field.name, field.width
            ));
        };
        row.insert(field.name.clone(), id_value(fresh));
    }
    Ok(row)
}

/// The id bytes a vocabulary handed out, as the tree variant the write path wants.
fn id_value(id: Vec<u8>) -> Value {
    match id.len() {
        1 => Value::U8(id[0]),
        2 => Value::U16(u16::from_be_bytes([id[0], id[1]])),
        4 => Value::U32(u32::from_be_bytes([id[0], id[1], id[2], id[3]])),
        8 => Value::U64(u64::from_be_bytes(
            id.as_slice().try_into().unwrap_or_default(),
        )),
        _ => Value::Bytes(id),
    }
}

fn is_id_face(schema: &okm_core::schema::CollectionSchema, slot: u16) -> bool {
    schema
        .hot_fields
        .iter()
        .chain(schema.cold_fields.iter())
        .any(|f| matches!(f.ty, okm_core::FieldType::Vocab(id) if id == slot))
}