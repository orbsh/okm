//! `okm-drain` — turn sealed change-log segments into windows of Parquet.
//!
//! The log is three things: a directory per table (`<root>/<header hex>/`), the
//! segments in it (`.segment` once sealed, `.partial` while being written), and a
//! schema sidecar holding the one thing the bytes cannot (`schema-v<N>.json`:
//! types and key layout). This crate is the second of ADR-0045's three layers —
//! it joins N sealed segments into ONE window and hands that window to a writer.
//! It is not the sink: the format, the partitioning and the file size belong to
//! the adapter on top (`okm-iceberg` and friends), and nothing here decides them.
//!
//! What a window IS: an append of changes. The same key may appear twice, because
//! a window is history, not a table — collapsing it into a current state is a
//! compaction the sink does (an upsert key, a delete file, a rewrite), not
//! something this crate can promise. What it refuses to do is pretend: an op it
//! cannot express as a row stops the window instead of being skipped, and a
//! dynamic-segment-only document is counted in the report rather than dropped in
//! silence.
//!
//! Ordering: commit first, then append (ADR-0045 §7), so a window can miss a tail
//! — never invent one. The watermark moves only after the Parquet file that
//! covers the window EXISTS, and it is replaced by rename, because a consumer
//! that took a half-written watermark would skip a window forever.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]

use okm_core::PRIMARY_SLOT;
use okm_core::changelog::read_sealed;
use okm_core::schema::CollectionSchema;
use okm_dynamic::arrow::record_batch;
use okm_wire::{OP_DELETE, OP_PUT};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// What a failed drain says. One shape for every layer below (io, JSON, codec,
/// Parquet) because the caller's decision is the same for all of them: stop, fix
/// the environment, run again — the watermark did not move.
#[derive(Debug)]
pub struct DrainError {
    pub what: String,
}

impl DrainError {
    fn new(what: impl Into<String>) -> Self {
        Self { what: what.into() }
    }
}

impl fmt::Display for DrainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "drain: {}", self.what)
    }
}

impl std::error::Error for DrainError {}

impl From<std::io::Error> for DrainError {
    fn from(e: std::io::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<okm_core::Error> for DrainError {
    fn from(e: okm_core::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<okm_dynamic::CodecError> for DrainError {
    fn from(e: okm_dynamic::CodecError) -> Self {
        Self::new(e.to_string())
    }
}

impl From<serde_json::Error> for DrainError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(format!("sidecar: {e}"))
    }
}

impl From<parquet::errors::ParquetError> for DrainError {
    fn from(e: parquet::errors::ParquetError) -> Self {
        Self::new(format!("parquet: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, DrainError>;

/// Where windows go, who is reading, and how many segments one window may hold.
///
/// `max_segments` is the ONE knob ADR-0045 §17 leaves to the drainer: a window is
/// also the amount of history a query must merge with the hot store, so bigger
/// windows mean fewer, larger files and a staler lake side.
#[derive(Clone, Debug)]
pub struct DrainConfig {
    pub out_dir: PathBuf,
    /// The consumer's name: its watermark file is per-consumer, because two
    /// readers at different positions are two positions, not a disagreement.
    pub consumer: String,
    pub max_segments: usize,
}

impl DrainConfig {
    pub fn new(out_dir: impl Into<PathBuf>, consumer: impl Into<String>) -> Self {
        Self {
            out_dir: out_dir.into(),
            consumer: consumer.into(),
            max_segments: 16,
        }
    }
}

/// What one drain did — enough to tell "nothing to do" from "everything worked".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub tables: usize,
    pub segments: usize,
    /// Rows written, i.e. one per captured put on the primary face. A window is
    /// history, so this counts changes, not distinct keys.
    pub rows: usize,
    /// Dynamic-segment ops the declaration has no columns for. Counted, never
    /// dropped in silence — the twin of the typed bridge's NULL payload half.
    pub segment_only: usize,
    pub files: Vec<PathBuf>,
}

/// What one window file holds, written beside it (`<same stem>.manifest.json`).
///
/// The Parquet file is self-describing about TYPES and says nothing about where it
/// came from: which log, which segments, how many rows, under which layout
/// version. That is what a sink needs to record a provenance entry without
/// reading the file, and what an operator needs to answer "did window 7 land".
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct WindowManifest {
    /// The table directory's name — its header in hex, the only name the log has.
    pub table: String,
    pub layout_version: u8,
    /// The segments this window covers, in order, as file names.
    pub segments: Vec<String>,
    pub rows: usize,
    /// `(name, arrow type, nullable)` in column order, keys first.
    pub columns: Vec<(String, String, bool)>,
    /// The Parquet file's name, beside this manifest.
    pub file: String,
}

/// Drain every table directory under `root`.
pub fn drain(root: &Path, cfg: &DrainConfig) -> Result<Report> {
    let mut report = Report::default();
    std::fs::create_dir_all(&cfg.out_dir)?;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        drain_table(&dir, cfg, &mut report)?;
    }
    Ok(report)
}

/// One window of captured changes, decoded as far as a declaration can take it.
///
/// This is the seam ADR-0056 §8 asks for: what the segments SAY is one thing, what
/// a sink can EXPRESS is another. A window carries rows AND removals; a Parquet
/// file can hold only the first, while a replay applies both in order. The
/// refusal therefore belongs to the sink, not to the reader — before this, one
/// delete anywhere in a window stopped the read itself.
pub struct Window {
    /// The declaration these bytes were written under, carried so a sink needs
    /// nothing but the window (and so a standalone tool needs no app types).
    pub schema: CollectionSchema,
    /// The table directory's name — its header in hex. The segment header will
    /// carry the declared name (ADR-0056 §7); until then this is the only one.
    pub table: String,
    pub layout_version: u8,
    /// The segments this window covers, in order, as file names.
    pub segments: Vec<String>,
    /// `(key suffix, document)` per put on the primary face, in order.
    pub rows: Vec<(Vec<u8>, okm_dynamic::ValueMap)>,
    /// `(slot, key suffix)` per removal, in order. Slot `0x00` means the ROW is
    /// gone; other slots are the document's own parts (purge takes both).
    pub deletes: Vec<(u16, Vec<u8>)>,
    /// Dynamic-segment ops the declaration has no columns for.
    pub segment_only: usize,
    /// `(slot, id) → label` for every vocabulary entry the window carried, so a
    /// reader never has to invent a name for an id.
    pub labels: HashMap<(u16, Vec<u8>), String>,
}

/// Read the next window for this consumer: every sealed segment after its
/// position, joined and decoded. `None` means there is nothing new.
pub fn read_window(dir: &Path, cfg: &DrainConfig) -> Result<Option<Window>> {
    let header = header_of(dir)?;
    let table = file_name(dir);
    let schema = load_sidecar(dir)?;

    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "segment"))
        .collect();
    segments.sort();

    let watermark = std::fs::read_to_string(watermark_path(cfg, &table))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // A sealed segment is immutable, so "after the watermark" is a name
    // comparison on zero-padded numbers: no counter, no gaps to reason about.
    let window: Vec<PathBuf> = segments
        .into_iter()
        .filter(|p| file_name(p) > watermark)
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
        segment_only: 0,
        labels: HashMap::new(),
    };

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
                        out.rows.push((
                            suffix.to_vec(),
                            okm_dynamic::decode_payload(&schema, &value)?,
                        ));
                    }
                    // A vocabulary entry: the id face the declaration names, with
                    // the label as its value. Carried because a column holds
                    // labels and the payload holds ids.
                    (OP_PUT, slot) if is_id_face(&schema, slot) => {
                        if let Ok(name) = String::from_utf8(value) {
                            out.labels.insert((slot, suffix.to_vec()), name);
                        }
                    }
                    (OP_PUT, _) => out.segment_only += 1,
                    // A removal is a fact this log carries and a column file
                    // cannot: recorded here, answered by whichever sink is
                    // reading (ADR-0056 §5).
                    (OP_DELETE, slot) => out.deletes.push((slot, suffix.to_vec())),
                    _ => {}
                }
            }
        }
    }
    Ok(Some(out))
}

/// The position file for one `(consumer, table)` pair.
///
/// Segment numbers restart per table, so the position must too: a consumer-only
/// name lets whichever table is furthest ahead move the mark past every lagging
/// table's window, silently and for good (ADR-0056 §8).
fn watermark_path(cfg: &DrainConfig, table: &str) -> PathBuf {
    cfg.out_dir
        .join(format!("watermark-{}-{table}.txt", cfg.consumer))
}

/// Render one window as a Parquet file plus its column list. The sink's rule
/// lives here: a removal has no column, and inventing a tombstone column would
/// change what every other reader of this file sees, so a window holding one is
/// refused instead of misrepresented.
pub fn write_parquet(window: &Window, out: &Path) -> Result<Vec<(String, String, bool)>> {
    if let Some((slot, key)) = window.deletes.first() {
        return Err(DrainError::new(format!(
            "{}: a delete op at slot 0x{slot:02X} ({key:02X?}) — a window is an append and \
             this sink cannot express a removal; replay it into a store, or compact the log first",
            window.table
        )));
    }
    let keys: Vec<Vec<u8>> = window.rows.iter().map(|(k, _)| k.clone()).collect();
    let rows: Vec<Option<okm_dynamic::ValueMap>> =
        window.rows.iter().map(|(_, d)| Some(d.clone())).collect();
    let batch = record_batch(
        &window.schema,
        &keys,
        &rows,
        Some(&|slot, id| window.labels.get(&(slot, id.to_vec())).cloned()),
    )?;
    okm_core::bridge::tooling::parquet_io::write_batch(&batch, out)?;
    Ok(batch
        .schema()
        .fields()
        .iter()
        .map(|f| {
            (
                f.name().clone(),
                format!("{:?}", f.data_type()),
                f.is_nullable(),
            )
        })
        .collect())
}

fn drain_table(dir: &Path, cfg: &DrainConfig, report: &mut Report) -> Result<()> {
    report.tables += 1;
    let Some(window) = read_window(dir, cfg)? else {
        return Ok(());
    };

    if window.rows.is_empty() {
        // Nothing a column can hold in this window: leave the position alone so
        // a later window that does carry rows still starts from here.
        report.segments += window.segments.len();
        return Ok(());
    }

    let first = stem_of(&window.segments[0]);
    let last = stem_of(&window.segments[window.segments.len() - 1]);
    let out = cfg
        .out_dir
        .join(format!("{}-{first}-{last}.parquet", window.table));
    let columns = write_parquet(&window, &out)?;

    // The manifest lands with the file: a sink records provenance from it without
    // opening the Parquet, and an operator answers "did window N land".
    let manifest = WindowManifest {
        table: window.table.clone(),
        layout_version: window.layout_version,
        segments: window.segments.clone(),
        rows: window.rows.len(),
        columns,
        file: file_name(&out),
    };
    let manifest_path = out.with_extension("manifest.json");
    write_atomic(&manifest_path, &serde_json::to_string_pretty(&manifest)?)?;

    // Only now: the file that covers the window exists, and so does its manifest.
    write_atomic(
        &watermark_path(cfg, &window.table),
        &format!("{last}.segment"),
    )?;

    report.segments += window.segments.len();
    report.rows += window.rows.len();
    report.segment_only += window.segment_only;
    report.files.push(out);
    Ok(())
}

/// The table directory's name IS its header in hex (`changelog::table_dir`), so
/// the drainer reads the header out of the path instead of being told it.
fn header_of(dir: &Path) -> Result<Vec<u8>> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| DrainError::new(format!("{}: not a table directory", dir.display())))?;
    if name.len() % 2 != 0 {
        return Err(DrainError::new(format!("{name}: odd-length hex header")));
    }
    (0..name.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&name[i..i + 2], 16)
                .map_err(|_| DrainError::new(format!("{name}: not a hex header")))
        })
        .collect()
}

/// The one declaration this directory's bytes are read through. Absent means the
/// log is readable as bytes and useless as columns, which is a stop, not a guess.
pub fn load_sidecar(dir: &Path) -> Result<CollectionSchema> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("schema-v") && n.ends_with(".json"))
        })
        .collect();
    found.sort();
    let path = found.first().ok_or_else(|| {
        DrainError::new(format!(
            "{}: no schema sidecar — nothing can turn these bytes into columns",
            dir.display()
        ))
    })?;
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// `key = header + slot + suffix`. The slot is one byte on the fixed faces and two
/// on the counting families (ADR-0044's numbering), which is why the width comes
/// from the first byte and not from a constant.
fn split_slot<'a>(header: &[u8], key: &'a [u8]) -> Option<(u16, &'a [u8])> {
    let rest = key.strip_prefix(header)?;
    let first = *rest.first()?;
    if first <= 0x0F {
        Some((u16::from(first), &rest[1..]))
    } else {
        let second = *rest.get(1)?;
        Some((u16::from_be_bytes([first, second]), &rest[2..]))
    }
}

fn is_id_face(schema: &CollectionSchema, slot: u16) -> bool {
    schema
        .hot_fields
        .iter()
        .chain(schema.cold_fields.iter())
        .any(|f| matches!(f.ty, okm_core::FieldType::Vocab(id) if id == slot))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// The stem of a segment file name: `00000007.segment` → `00000007`.
fn stem_of(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Replace by rename: a reader either sees the old watermark or the new one.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
