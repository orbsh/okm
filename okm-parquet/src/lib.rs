//! `okm-parquet` — project the change log into Parquet files (ADR-0056 §8).
//!
//! This is the FORMAT half. The mechanism — segments, the window join, the
//! watermark, the half of the declaration the log cannot carry — lives in
//! `okm_core::changelog`: a window is what a segment is FOR, so producing one is
//! core's work. What is left here is the projection: Arrow columns, a Parquet file,
//! a manifest beside it, and the position that says which window it covered.
//!
//! That is also why this is a standalone binary. It reads a table directory
//! (segments + sidecars) and needs nothing from the application that wrote it: the
//! table name comes from the segment header, the columns from the sidecar. The old
//! "the app hands the exporter an interface" requirement belonged to a store-scan
//! export, not to this.
//!
//! What a window IS: an append of changes. The same key may appear twice, because a
//! window is history, not a table — collapsing it into a current state is a
//! compaction the sink does (an upsert key, a delete file, a rewrite), not
//! something this crate can promise. What it refuses to do is pretend: a window
//! carrying a removal is refused rather than written without it, and a
//! dynamic-segment-only document is counted in the report rather than dropped in
//! silence.
//!
//! Ordering runs one way only: the watermark moves after the Parquet file and its
//! manifest EXIST, because a consumer that took a half-written position would skip
//! a window forever.

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

pub mod bridge;
pub mod delta;
pub mod read;
pub mod snapshot;

use okm_core::changelog::window::{
    self, Window, WindowConfig, WindowOp, read_watermark, table_dirs, write_watermark,
};
use okm_dynamic::arrow::record_batch;
use std::fmt;
use std::path::{Path, PathBuf};

/// What a failed projection says. One shape for every layer below (io, JSON,
/// codec, Parquet) because the caller's decision is the same for all of them:
/// stop, fix the environment, run again — the watermark did not move.
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
        write!(f, "okm-parquet: {}", self.what)
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
        Self::new(format!("manifest: {e}"))
    }
}

impl From<parquet::errors::ParquetError> for DrainError {
    fn from(e: parquet::errors::ParquetError) -> Self {
        Self::new(format!("parquet: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, DrainError>;

/// When a drained table is worth folding into one current-state file (ADR-0058 § Amendment).
///
/// A fold is O(table), so a service should not do it per window: it fires when the change
/// files have piled up — by count, by bytes, or both (`0` disables one of them). What it
/// buys is a reader that does not have to replay every change row of every window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldPolicy {
    /// Fold when the table holds at least this many change files (`0`: never by count).
    pub after_files: usize,
    /// …or when those files add up to at least this many bytes (`0`: never by size).
    pub after_bytes: u64,
}

impl FoldPolicy {
    /// Fold every `n` change files.
    pub fn files(n: usize) -> Self {
        Self {
            after_files: n,
            after_bytes: 0,
        }
    }

    /// Fold once the change files pass `n` bytes.
    pub fn bytes(n: u64) -> Self {
        Self {
            after_files: 0,
            after_bytes: n,
        }
    }

    fn met(&self, files: usize, bytes: u64) -> bool {
        (self.after_files > 0 && files >= self.after_files)
            || (self.after_bytes > 0 && bytes >= self.after_bytes)
    }
}

/// Where windows go, who is reading, and how many segments one window may hold.
///
/// `max_segments` is the ONE knob ADR-0045 §17 leaves to the drainer: a window is
/// also the amount of history a query must merge with the hot store, so bigger
/// windows mean fewer, larger files and a staler lake side.
#[derive(Clone, Debug)]
pub struct DrainConfig {
    pub out_dir: PathBuf,
    /// The consumer's name: its position file is per-consumer, because two readers
    /// at different positions are two positions, not a disagreement.
    pub consumer: String,
    pub max_segments: usize,
    /// Fold a table into one current-state file when its change files pile up. `None` (the
    /// default) leaves the table alone: a caller that wants current state on demand runs
    /// `okm-parquet condense` instead.
    pub fold: Option<FoldPolicy>,
}

impl DrainConfig {
    pub fn new(out_dir: impl Into<PathBuf>, consumer: impl Into<String>) -> Self {
        Self {
            out_dir: out_dir.into(),
            consumer: consumer.into(),
            max_segments: 16,
            fold: None,
        }
    }

    /// The reading half's own config for one table: the window size and this
    /// consumer's position.
    fn window_for(&self, table: &str) -> Result<WindowConfig> {
        Ok(WindowConfig {
            max_segments: self.max_segments,
            after: read_watermark(&self.out_dir, &self.consumer, table)?,
        })
    }
}

/// What one pass did — enough to tell "nothing to do" from "everything worked".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub tables: usize,
    pub segments: usize,
    /// Rows written, i.e. one per captured put on the primary face. A window is
    /// history, so this counts changes, not distinct keys.
    pub rows: usize,
    /// Removals written as change rows (`okm_op = del`) — the half a column file
    /// could not state before ADR-0058 made the export a change table.
    pub deletes: usize,
    /// Removals of derived state (an index entry, a ledger line): counted, not written,
    /// because they carry no key of the table's shape and the write path rebuilds them.
    pub derived_deletes: usize,
    /// Dynamic-segment puts carried as the window's `variant` column. Counted so a
    /// sink can tell "no open fields anywhere" from "open fields written".
    pub open_fields: usize,
    pub files: Vec<PathBuf>,
    /// Delta commits written (one per table that produced a window).
    pub metadata: usize,
    /// Tables folded into one current-state file by the drain's fold policy.
    pub folds: usize,
    /// Tables the policy found nothing to do for — already current, or a fold this reader
    /// refuses (open fields written by more than one window). The table is untouched and
    /// the drain is not in error.
    pub fold_skipped: usize,
}

/// What one window file holds, written beside it (`<same stem>.manifest.json`).
///
/// The Parquet file is self-describing about TYPES and says nothing about where it
/// came from: which log, which segments, how many rows, under which layout version.
/// That is what a sink needs to record a provenance entry without reading the file,
/// and what an operator needs to answer "did window 7 land".
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct WindowManifest {
    /// The table directory's name — its header in hex.
    pub table: String,
    pub layout_version: u8,
    /// The segments this window covers, in order, as file names.
    pub segments: Vec<String>,
    pub rows: usize,
    /// Change rows carrying `okm_op = del`, so a sink records provenance without
    /// opening the file (`#[serde(default)]`: manifests written before ADR-0058 have
    /// no removals to count).
    #[serde(default)]
    pub deletes: usize,
    /// `(name, arrow type, nullable)` in column order, keys first.
    pub columns: Vec<(String, String, bool)>,
    /// The Parquet file's name, beside this manifest.
    pub file: String,
}

/// Project every table directory under `root`.
pub fn drain(root: &Path, cfg: &DrainConfig) -> Result<Report> {
    let mut report = Report::default();
    std::fs::create_dir_all(&cfg.out_dir)?;
    for dir in table_dirs(root)? {
        drain_table(&dir, cfg, &mut report)?;
    }
    Ok(report)
}

/// Project one table directory: every window this consumer has not taken.
///
/// A window is ONE generation (ADR-0056 §6), and a table's directory outlives every build
/// that wrote to it, so one pass over a directory that has seen an upgrade is several
/// windows. Looping here is what keeps a single invocation from advancing the position to
/// the end of one generation and reporting success.
pub fn drain_table(dir: &Path, cfg: &DrainConfig, report: &mut Report) -> Result<()> {
    report.tables += 1;
    let table = window::file_name(dir);
    while let Some(win) = window::read_window(dir, &cfg.window_for(&table)?)? {
        if win.rows.is_empty()
            && win.dynamic.is_empty()
            && !win
                .deletes
                .iter()
                .any(|(_, key)| key.len() == win.schema.key_len)
        {
            // Nothing this file can state: no row, no open fields, no removal of a key.
            // Leave the position alone so a later window that does carry changes still
            // starts from here — and stop, since an unmoved position would make this loop
            // run for ever. A window holding only a REMOVAL is not this case: a change
            // table states it (ADR-0058), and the loop must take it.
            report.segments += win.segments.len();
            break;
        }
        drain_one(cfg, report, &win)?;
    }
    maybe_fold(cfg, &table, report)
}

/// Fold the table if the policy says its change files have piled up.
///
/// Runs after the windows are committed, never inside one: a fold is a replacement of files a
/// reader can already see, so it must not be part of the half-commit rule that guards a
/// window. A table below the threshold (or already folded) is left exactly as it is.
fn maybe_fold(cfg: &DrainConfig, table: &str, report: &mut Report) -> Result<()> {
    let Some(policy) = cfg.fold else {
        return Ok(());
    };
    let table_dir = cfg.out_dir.join(table);
    if !table_dir.join("_delta_log").is_dir() {
        return Ok(());
    }
    let files =
        delta::files_in_order(&table_dir).map_err(|e| DrainError::new(format!("delta: {e}")))?;
    let bytes: u64 = files
        .iter()
        .filter_map(|rel| std::fs::metadata(table_dir.join(rel)).ok())
        .map(|m| m.len())
        .sum();
    if !policy.met(files.len(), bytes) {
        return Ok(());
    }
    match crate::read::condense(&table_dir) {
        Ok(_) => {
            report.folds += 1;
            Ok(())
        }
        // The table is fine and this fold is not: already current, or state this reader
        // cannot compress without dropping something (two windows of open fields). A policy
        // skips it and keeps draining; only a broken table stops the run.
        Err(e) if e.refused => {
            report.fold_skipped += 1;
            Ok(())
        }
        Err(e) => Err(DrainError::new(e.what)),
    }
}

/// One window: the file, its manifest, the position, and the Delta commit.
fn drain_one(cfg: &DrainConfig, report: &mut Report, win: &Window) -> Result<()> {
    let first = window::stem_of(&win.segments[0]);
    let last = window::stem_of(&win.segments[win.segments.len() - 1]);
    // One directory per table, `data/` inside it: that directory IS the exported
    // table (ADR-0056 §8), and a reader is pointed at it rather than at a pile of
    // files that also serves other tables.
    let table_dir = cfg.out_dir.join(&win.table);
    let data_dir = table_dir.join("data");
    let out = data_dir.join(format!("{}-{first}-{last}.parquet", win.table));

    // Written into a scratch name beside the watermark and moved into place only
    // once the projection succeeded: a refused window (a delete has no column) must
    // leave no directory tree, no file and no moved position behind, which is the
    // half-commit rule the tests hold this to.
    let scratch = cfg
        .out_dir
        .join(format!(".{}-{first}-{last}.parquet.partial", win.table));
    let projection = write_parquet(win, &scratch)?;
    std::fs::create_dir_all(&data_dir)?;
    std::fs::rename(&scratch, &out)?;

    // The manifest lands with the file: a sink records provenance from it without
    // opening the Parquet, and an operator answers "did window N land".
    let manifest = WindowManifest {
        table: win.table.clone(),
        layout_version: win.layout_version,
        segments: win.segments.clone(),
        rows: projection.rows,
        deletes: projection.deletes,
        columns: projection.columns,
        file: window::file_name(&out),
    };
    write_atomic(
        &out.with_extension("manifest.json"),
        &serde_json::to_string_pretty(&manifest)?,
    )?;

    // Only now: the file that covers the window exists, and so does its manifest.
    write_watermark(
        &cfg.out_dir,
        &cfg.consumer,
        &win.table,
        &format!("{last}.segment"),
    )?;

    // The Delta view: this window is one more `add` in the log, not a rebuild of
    // everything before it. Committed after the position moves, because a log entry
    // describes a file that exists. Every file in `data/` is offered so a deleted or
    // never-written log heals itself; the ones already recorded are skipped.
    let files: Vec<PathBuf> = std::fs::read_dir(&data_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .collect();
    // The key columns travel into the log's schema metadata (`okm.key`): a reader that
    // folds history needs the table's identity, and a file cannot say which of its columns
    // that is (ADR-0058 § Amendment).
    let keys: Vec<String> = win
        .schema
        .key_fields
        .iter()
        .map(|f| f.name.clone())
        .collect();
    delta::commit_files(&table_dir, &files, &keys)
        .map_err(|e| DrainError::new(format!("delta: {e}")))?;
    report.metadata += 1;

    report.segments += win.segments.len();
    report.rows += win.rows.len();
    report.deletes += projection.deletes;
    report.derived_deletes += projection.derived_deletes;
    report.open_fields += win.dynamic.len();
    report.files.push(out);
    Ok(())
}

/// The op names a change row can carry in `okm_op`.
pub const OKM_PUT: &str = "put";
/// A change that touched only the document's open fields: the declared columns are NULL
/// because the window did not carry them, and the `variant` cell holds what it did.
pub const OKM_PUT_OPEN: &str = "put_open";
/// A removal. `okm_slot` says which face it touched; `0` is the whole row.
pub const OKM_DEL: &str = "del";

/// What projecting one window produced, beyond the columns: how many rows landed and of
/// which kind, so a sink reads it instead of counting the file again.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Projection {
    /// `(name, arrow type, nullable)` in column order, keys first.
    pub columns: Vec<(String, String, bool)>,
    /// Rows in the file: puts, open-field-only puts and removals.
    pub rows: usize,
    /// `del` rows.
    pub deletes: usize,
    /// `put_open` rows.
    pub open_only: usize,
    /// Removals of DERIVED state (an index entry, a ledger line). They carry no key of
    /// the table's shape and are rebuilt from the row, so they are counted rather than
    /// written — the same treatment the puts of those faces already get.
    pub derived_deletes: usize,
}

/// Render one window as a Parquet file plus what it holds.
///
/// The file is a CHANGE set, and it says so in three bookkeeping columns no declaration
/// has a field for: `okm_op` (`put`, `put_open`, `del`), `okm_slot` (which face a removal
/// touched; `0` is the whole row) and `okm_seq` (the op's position inside the window).
/// Without them a removal had nowhere to go and the sink refused the whole window; with
/// them every window is expressible, and a reader that wants current state filters on
/// `okm_op` and keeps the last `okm_seq` per key (ADR-0058).
///
/// Two things are deliberately NOT rows: a removal of derived state (no key of the
/// table's shape — counted, not dropped in silence) and the puts of those same faces
/// (already counted as `open_fields` by the window join). Both are rebuilt from the row.
pub fn write_parquet(win: &Window, out: &Path) -> Result<Projection> {
    let key_len = win.schema.key_len;
    let mut keys: Vec<Vec<u8>> = Vec::with_capacity(win.ops.len());
    let mut rows: Vec<Option<okm_dynamic::ValueMap>> = Vec::with_capacity(win.ops.len());
    let mut variant_of: Vec<Option<&okm_dynamic::ValueMap>> = Vec::with_capacity(win.ops.len());
    let mut op_names: Vec<&str> = Vec::with_capacity(win.ops.len());
    let mut slots: Vec<Option<u16>> = Vec::with_capacity(win.ops.len());
    let mut seqs: Vec<u32> = Vec::with_capacity(win.ops.len());
    let mut projection = Projection::default();

    // Open fields written under a key in this window are part of the same change as the
    // row, so a put carries them in its `variant` cell (ADR-0045 §4: truth, never
    // re-derived from the columns).
    let by_key: std::collections::HashMap<&[u8], &okm_dynamic::ValueMap> = win
        .dynamic
        .iter()
        .map(|(key, map)| (key.as_slice(), map))
        .collect();

    // Keys this window put as a row: their open fields ride on the row's `variant` cell,
    // so the same change is not written twice.
    let put_keys: std::collections::HashSet<&[u8]> = win
        .ops
        .iter()
        .filter_map(|op| match op {
            WindowOp::Put(i) => Some(win.rows[*i].0.as_slice()),
            _ => None,
        })
        .collect();

    for (seq, op) in win.ops.iter().enumerate() {
        let seq = u32::try_from(seq).map_err(|_| {
            DrainError::new(format!(
                "{}: a window cannot hold more than 2^32 operations",
                win.table
            ))
        })?;
        match *op {
            WindowOp::Put(i) => {
                let (key, doc) = &win.rows[i];
                variant_of.push(by_key.get(key.as_slice()).copied());
                keys.push(key.clone());
                rows.push(Some(doc.clone()));
                op_names.push(OKM_PUT);
                slots.push(None);
            }
            WindowOp::Del(i) => {
                let (slot, key) = &win.deletes[i];
                if key.len() != key_len {
                    // Derived state (an index entry, a ledger line): no key of the
                    // table's shape, and the write path rebuilds it from the row.
                    projection.derived_deletes += 1;
                    continue;
                }
                variant_of.push(None);
                keys.push(key.clone());
                rows.push(None);
                op_names.push(OKM_DEL);
                slots.push(Some(*slot));
            }
            WindowOp::Dynamic(i) => {
                let (key, map) = &win.dynamic[i];
                if put_keys.contains(key.as_slice()) {
                    // The row put in this same window already carries these open fields
                    // in its `variant` cell; a second row would state one change twice.
                    continue;
                }
                variant_of.push(Some(map));
                keys.push(key.clone());
                rows.push(None);
                op_names.push(OKM_PUT_OPEN);
                slots.push(None);
            }
        }
        seqs.push(seq);
    }

    let mut batch = record_batch(
        &win.schema,
        &keys,
        &rows,
        Some(&|slot, id| win.labels.get(&(slot, id.to_vec())).cloned()),
    )?;

    // The open half: every document's dynamic segment as ONE `variant` column — the same
    // name and the same encoding ADR-0034 gives the typed export, because it is the same
    // column and a reader should not meet two names for it.
    if !win.dynamic.is_empty() {
        let (field, column) = crate::bridge::variant_column("variant", &variant_of);
        let mut fields: Vec<arrow::datatypes::Field> = batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        fields.push(field);
        let mut arrays: Vec<std::sync::Arc<dyn arrow::array::Array>> = batch.columns().to_vec();
        arrays.push(column);
        batch = arrow::array::RecordBatch::try_new(
            std::sync::Arc::new(arrow::datatypes::Schema::new(fields)),
            arrays,
        )
        .map_err(|e| DrainError::new(format!("{}: open column: {e}", win.table)))?;
    }

    // The change columns, after every column the declaration has. The names carry a
    // prefix because they are not the document's: they are what the log said about it.
    let mut fields: Vec<arrow::datatypes::Field> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    let mut arrays: Vec<std::sync::Arc<dyn arrow::array::Array>> = batch.columns().to_vec();
    fields.push(arrow::datatypes::Field::new(
        "okm_op",
        arrow::datatypes::DataType::Utf8,
        false,
    ));
    arrays.push(std::sync::Arc::new(arrow::array::StringArray::from(
        op_names.clone(),
    )));
    fields.push(arrow::datatypes::Field::new(
        "okm_slot",
        arrow::datatypes::DataType::UInt16,
        true,
    ));
    arrays.push(std::sync::Arc::new(arrow::array::UInt16Array::from(slots)));
    fields.push(arrow::datatypes::Field::new(
        "okm_seq",
        arrow::datatypes::DataType::UInt32,
        false,
    ));
    arrays.push(std::sync::Arc::new(arrow::array::UInt32Array::from(seqs)));
    let batch = arrow::array::RecordBatch::try_new(
        std::sync::Arc::new(arrow::datatypes::Schema::new(fields)),
        arrays,
    )
    .map_err(|e| DrainError::new(format!("{}: change columns: {e}", win.table)))?;

    crate::snapshot::parquet_io::write_batch(&batch, out)?;

    projection.rows = op_names.len();
    projection.deletes = op_names.iter().filter(|n| **n == OKM_DEL).count();
    projection.open_only = op_names.iter().filter(|n| **n == OKM_PUT_OPEN).count();
    projection.columns = batch
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
        .collect();
    Ok(projection)
}

/// Replace by rename: a reader either sees the old file or the new one.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
