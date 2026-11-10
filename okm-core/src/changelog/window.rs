//! The window join (ADR-0056 §8): sealed segments + the declaration → one window
//! of changes, and the consumer position that says where the last window ended.
//!
//! Why this is core and not a downstream crate: a window is what a segment is FOR.
//! The bytes, the ordering and the half of the schema the log cannot carry are all
//! decided here, so the join lives beside the readers instead of re-deriving them
//! one directory up. What leaves the crate is only the FORMAT — Arrow columns,
//! Parquet, the table log — and each of those is a projection of a window, not part
//! of producing one. Replay does NOT leave: applying a segment through a target's
//! write path is mechanism, so it lives beside this join (`super::replay`), behind a
//! trait the crate above implements.
//!
//! What a window IS: an append of changes. The same key may appear twice, because a
//! window is history, not a table — collapsing it is a compaction the sink does.
//! What it refuses to do is pretend: an op it cannot express as a change stops the
//! window instead of being skipped, and a dynamic segment travels in
//! [`Window::dynamic`] rather than being dropped in silence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::obj_dynamic::ValueMap;
use crate::schema::CollectionSchema;
use crate::model::index::DYNAMIC_SLOT;
use crate::{OP_DELETE, OP_PUT, PRIMARY_SLOT};

use super::{read_sealed, table_dir};

/// One window of captured changes, decoded as far as a declaration can take it.
///
/// This is the seam ADR-0056 §5 asks for: what the segments SAY is one thing, what
/// a sink can EXPRESS is another. A window carries rows AND removals; a column file
/// can hold only the first, while a replay applies both in order. The refusal
/// therefore belongs to the sink, not to the reader.
pub struct Window {
    /// The declaration these bytes were written under, carried so a sink needs
    /// nothing but the window (and so a standalone tool needs no application types).
    pub schema: CollectionSchema,
    /// The table directory's name — its header in hex.
    pub table: String,
    pub layout_version: u8,
    /// The segments this window covers, in order, as file names.
    pub segments: Vec<String>,
    /// `(key suffix, document)` per put on the primary face, in order.
    pub rows: Vec<(Vec<u8>, ValueMap)>,
    /// `(slot, key suffix)` per removal, in order. Slot `0x00` means the ROW is
    /// gone; other slots are the document's own parts (purge takes both).
    pub deletes: Vec<(u16, Vec<u8>)>,
    /// `(key suffix, open fields)` per dynamic-segment put: the names the
    /// declaration does NOT have, and their values. A window is a row view, so these
    /// are not columns — they travel as one `variant` column beside them (the
    /// analytics twin of a segment), and a key with no primary row carries no row
    /// here at all (a document with no row is not a row).
    pub dynamic: Vec<(Vec<u8>, ValueMap)>,
    /// Ops that carry no column and no segment of their own (an index entry, a
    /// reduce ledger line): the write path rebuilds them from the row.
    pub open_fields: usize,
    /// `(slot, id) → label` for every vocabulary entry the window carried, so a
    /// reader never has to invent a name for an id.
    pub labels: HashMap<(u16, Vec<u8>), String>,
}

/// How much of a table's log one window takes.
#[derive(Clone, Debug)]
pub struct WindowConfig {
    /// Stop after this many segments. A window is also the amount of history a
    /// query merges with the hot store, so this is the drainer's one knob
    /// (ADR-0045 §17).
    pub max_segments: usize,
    /// The consumer's position: the last segment name it took (`None` = from the
    /// beginning). A name comparison on zero-padded numbers — no counter, no gaps.
    pub after: Option<String>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            max_segments: 16,
            after: None,
        }
    }
}

/// Read the next window for a consumer: every sealed segment after its position,
/// joined and decoded. `Ok(None)` means there is nothing new.
pub fn read_window(dir: &Path, cfg: &WindowConfig) -> Result<Option<Window>> {
    let header = header_of(dir)?;
    let table = file_name(dir);
    let schema = load_sidecar(dir)?;

    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| io_err(dir, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            // A baseline is a snapshot, not a window of changes: replay applies it
            // whole (ADR-0056 §3), so the delta join must not treat it as one.
            let name = file_name(p);
            name.ends_with(super::SEALED_EXT) && !name.ends_with(super::BASELINE_SUFFIX)
        })
        .collect();
    segments.sort();

    let after = cfg.after.clone().unwrap_or_default();
    let window: Vec<PathBuf> = segments
        .into_iter()
        .filter(|p| file_name(p) > after)
        .take(cfg.max_segments.max(1))
        .collect();
    if window.is_empty() {
        return Ok(None);
    }

    let mut out = Window {
        schema: schema.clone(),
        table,
        layout_version: schema.layout_version,
        segments: window.iter().map(|p| file_name(p)).collect(),
        rows: Vec::new(),
        deletes: Vec::new(),
        dynamic: Vec::new(),
        open_fields: 0,
        labels: HashMap::new(),
    };
    // Dynamic bodies are decoded AFTER the window is scanned: their field ids need
    // the field-name vocabulary's labels, and nothing promises that entry was
    // written before the segment (frames are in write order, not dependency order).
    let mut dynamic_raw: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    for segment in &window {
        for frame in read_sealed(segment)? {
            for (tag, key, value) in frame.0 {
                let Some((slot, suffix)) = split_slot(&header, &key) else {
                    continue;
                };
                match (tag, slot) {
                    // A row: the primary face, and a suffix that is exactly one
                    // key (shorter would be a bare face, longer cannot happen).
                    (OP_PUT, PRIMARY_SLOT) if suffix.len() == schema.key_len => {
                        out.rows.push((suffix.to_vec(), crate::decode::decode_payload(
                            &schema, &value,
                        )?));
                    }
                    // A vocabulary entry: the id face the declaration names, with
                    // the label as its value. Carried because a column holds labels
                    // and the payload holds ids.
                    (OP_PUT, slot) if is_id_face(&schema, slot) => {
                        if let Ok(name) = String::from_utf8(value) {
                            out.labels.insert((slot, suffix.to_vec()), name);
                        }
                    }
                    // A document's open fields: truth (ADR-0045 §4), carried
                    // beside the row, never re-derived from it.
                    (OP_PUT, DYNAMIC_SLOT) if suffix.len() == schema.key_len => {
                        dynamic_raw.push((suffix.to_vec(), value.to_vec()));
                    }
                    (OP_PUT, _) => out.open_fields += 1,
                    // A removal is a fact this log carries and a column file
                    // cannot: recorded here, answered by whichever sink is reading
                    // (ADR-0056 §5).
                    (OP_DELETE, slot) => out.deletes.push((slot, suffix.to_vec())),
                    _ => {}
                }
            }
        }
    }
    for (key, body) in dynamic_raw {
        out.dynamic
            .push((key, decode_dynamic_segment(&body, &out.labels)?));
    }
    Ok(Some(out))
}

/// Decode a dynamic-segment body into `name → value`, taking the names from the
/// window's own field-vocabulary labels (top-level frames carry ids; nested `Obj`s
/// are already name-keyed).
///
/// A frame whose id has no label is an error rather than a skipped field: an unnamed
/// field cannot be written anywhere, and a reader that silently drops it produces a
/// document that looks complete and is not.
pub fn decode_dynamic_segment(
    body: &[u8],
    labels: &HashMap<(u16, Vec<u8>), String>,
) -> Result<ValueMap> {
    let faces = crate::obj_vocab::FIELD_VOCAB;
    let mut row = ValueMap::new();
    for field in crate::model::obj_dynamic::decode_named(body, &mut |id| {
        labels.get(&(faces.id_slot, id.to_be_bytes().to_vec())).cloned()
    }) {
        let Some(name) = labels
            .get(&(faces.id_slot, field.id.to_be_bytes().to_vec()))
            .cloned()
        else {
            return Err(crate::Error::Codec(
                crate::model::obj_dynamic::CodecError::UnknownField(format!(
                    "dynamic field id {} (no label in this window's field vocabulary)",
                    field.id
                )),
            ));
        };
        row.insert(name, field.value);
    }
    Ok(row)
}

/// The position file for one `(consumer, table)` pair.
///
/// Segment numbers restart per table, so the position must too: a consumer-only
/// name lets whichever table is furthest ahead move the mark past every lagging
/// table's window, silently and for good (ADR-0056 §8).
pub fn watermark_path(state_dir: &Path, consumer: &str, table: &str) -> PathBuf {
    state_dir.join(format!("watermark-{consumer}-{table}.txt"))
}

/// Where this consumer left off: the last segment name it took, or `None`.
pub fn read_watermark(state_dir: &Path, consumer: &str, table: &str) -> Result<Option<String>> {
    let path = watermark_path(state_dir, consumer, table);
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(&path, e)),
    }
}

/// Move the position. Replace by rename: a reader sees the old mark or the new
/// one, never a half-written line that would skip a window forever.
pub fn write_watermark(state_dir: &Path, consumer: &str, table: &str, to: &str) -> Result<()> {
    let path = watermark_path(state_dir, consumer, table);
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, to).map_err(|e| io_err(&tmp, e))?;
    std::fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
}

/// The table directory's name IS its header in hex (`changelog::table_dir`), so a
/// reader takes the header out of the path instead of being told it.
pub fn header_of(dir: &Path) -> Result<Vec<u8>> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| corrupt(dir, "not a table directory"))?;
    if name.len() % 2 != 0 {
        return Err(corrupt(dir, "odd-length hex header"));
    }
    (0..name.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&name[i..i + 2], 16).map_err(|_| corrupt(dir, "not a hex header"))
        })
        .collect()
}

/// The one declaration this directory's bytes are read through. Absent means the
/// log is readable as bytes and useless as columns, which is a stop, not a guess.
pub fn load_sidecar(dir: &Path) -> Result<CollectionSchema> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| io_err(dir, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("schema-v") && n.ends_with(".json"))
        })
        .collect();
    found.sort();
    let path = found
        .first()
        .ok_or_else(|| corrupt(dir, "no schema sidecar — nothing can turn these bytes into columns"))?;
    let text = std::fs::read_to_string(path).map_err(|e| io_err(path, e))?;
    serde_json::from_str(&text).map_err(|e| Error::Changelog {
        what: format!("{}: sidecar: {e}", path.display()),
    })
}

/// `key = header + slot + suffix`. The slot is one byte on the fixed faces and two
/// on the counting families (ADR-0044's numbering), which is why the width comes
/// from the first byte and not from a constant.
pub fn split_slot<'a>(header: &[u8], key: &'a [u8]) -> Option<(u16, &'a [u8])> {
    let rest = key.strip_prefix(header)?;
    let first = *rest.first()?;
    if first <= 0x0F {
        Some((u16::from(first), rest.get(1..)?))
    } else {
        let second = *rest.get(1)?;
        Some((u16::from_be_bytes([first, second]), rest.get(2..)?))
    }
}

/// Is this slot a vocabulary's id face, per the declaration?
pub fn is_id_face(schema: &CollectionSchema, slot: u16) -> bool {
    // The shared vocabularies too: the field-name face (0x02) is what names a
    // dynamic segment's frames, and leaving it out made every open field unnamed —
    // the same gap replay had, in the same place (a per-field list mistaken for the
    // whole set of vocabulary faces).
    slot == schema.slots.field_vocab_id
        || slot == schema.slots.predicate_vocab_id
        || schema
            .hot_fields
            .iter()
            .chain(schema.cold_fields.iter())
            .any(|f| matches!(f.ty, crate::field::FieldType::Vocab(id) if id == slot))
}

/// The stem of a segment file name: `00000007.segment` → `00000007`.
pub fn stem_of(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// A file name as text (the log's own names are ASCII by construction).
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Every table directory under a log root, in name order — the units one drain
/// pass walks.
pub fn table_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|e| io_err(root, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// The directory a table header owns, re-exported so a tool that has a header does
/// not have to hard-code the hex rule.
pub fn dir_of(root: &Path, header: &[u8]) -> PathBuf {
    table_dir(root, header)
}

fn io_err(path: &Path, e: std::io::Error) -> Error {
    Error::Changelog {
        what: format!("{}: {e}", path.display()),
    }
}

fn corrupt(path: &Path, what: &str) -> Error {
    Error::Changelog {
        what: format!("{}: {what}", path.display()),
    }
}