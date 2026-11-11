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
pub mod snapshot;

use okm_core::changelog::window::{
    self, Window, WindowConfig, read_watermark, table_dirs, write_watermark,
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
}

impl DrainConfig {
    pub fn new(out_dir: impl Into<PathBuf>, consumer: impl Into<String>) -> Self {
        Self {
            out_dir: out_dir.into(),
            consumer: consumer.into(),
            max_segments: 16,
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
    /// Dynamic-segment puts carried as the window's `variant` column. Counted so a
    /// sink can tell "no open fields anywhere" from "open fields written".
    pub open_fields: usize,
    pub files: Vec<PathBuf>,
    /// Delta commits written (one per table that produced a window).
    pub metadata: usize,
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
    loop {
        let Some(win) = window::read_window(dir, &cfg.window_for(&table)?)? else {
            return Ok(());
        };
        if win.rows.is_empty() {
            // Nothing a column can hold in this window: leave the position alone so a
            // later window that does carry rows still starts from here — and stop, since
            // an unmoved position would make this loop run for ever.
            report.segments += win.segments.len();
            return Ok(());
        }
        drain_one(cfg, report, &win)?;
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
    let columns = write_parquet(win, &scratch)?;
    std::fs::create_dir_all(&data_dir)?;
    std::fs::rename(&scratch, &out)?;

    // The manifest lands with the file: a sink records provenance from it without
    // opening the Parquet, and an operator answers "did window N land".
    let manifest = WindowManifest {
        table: win.table.clone(),
        layout_version: win.layout_version,
        segments: win.segments.clone(),
        rows: win.rows.len(),
        columns,
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
    delta::commit_files(&table_dir, &files).map_err(|e| DrainError::new(format!("delta: {e}")))?;
    report.metadata += 1;

    report.segments += win.segments.len();
    report.rows += win.rows.len();
    report.open_fields += win.dynamic.len();
    report.files.push(out);
    Ok(())
}

/// Render one window as a Parquet file plus its column list. The sink's rule lives
/// here: a removal has no column, and inventing a tombstone column would change
/// what every other reader of this file sees, so a window holding one is refused
/// instead of misrepresented.
pub fn write_parquet(win: &Window, out: &Path) -> Result<Vec<(String, String, bool)>> {
    if let Some((slot, key)) = win.deletes.first() {
        return Err(DrainError::new(format!(
            "{}: a delete op at slot 0x{slot:02X} ({key:02X?}) — a window is an append and \
             this sink cannot express a removal; replay it into a store, or compact the log first",
            win.table
        )));
    }
    let keys: Vec<Vec<u8>> = win.rows.iter().map(|(k, _)| k.clone()).collect();
    let rows: Vec<Option<okm_dynamic::ValueMap>> =
        win.rows.iter().map(|(_, d)| Some(d.clone())).collect();
    let mut batch = record_batch(
        &win.schema,
        &keys,
        &rows,
        Some(&|slot, id| win.labels.get(&(slot, id.to_vec())).cloned()),
    )?;

    // The open half: every document's dynamic segment as ONE `variant` column — the
    // same name and the same encoding ADR-0034 gives the typed export, because it is
    // the same column and a reader should not meet two names for it. The declared
    // fields have columns; the names the declaration does not have carry none, and a
    // window that wrote only them would hand a reader every row minus part of itself
    // (ADR-0045 §4 — the segment is truth).
    if !win.dynamic.is_empty() {
        let mut by_key: std::collections::HashMap<&[u8], &okm_dynamic::ValueMap> =
            std::collections::HashMap::with_capacity(win.dynamic.len());
        for (key, map) in &win.dynamic {
            by_key.insert(key.as_slice(), map);
        }
        // An open-fields segment whose row is NOT in this window: the row was removed
        // (or never existed here) and the segment outlived it. Writing the rows and
        // skipping the segment drops truth silently; attaching it to nothing is not an
        // option either, so this is the same stop as a delete — a window is a row view
        // and cannot express a removal (ADR-0056 §5).
        if let Some((key, _)) = win
            .dynamic
            .iter()
            .find(|(key, _)| !keys.iter().any(|k| k == key))
        {
            return Err(DrainError::new(format!(
                "{}: an open-fields segment for key {key:02X?} with no row in this window \
                 — the row is gone (a removal) and a window cannot express that; replay it \
                 into a store, or compact the log first",
                win.table
            )));
        }
        let maps: Vec<Option<&okm_dynamic::ValueMap>> = keys
            .iter()
            .map(|k| by_key.get(k.as_slice()).copied())
            .collect();
        // A dynamic-only document (a key with no row) has no row here: this file is a
        // row view, and the key is only in the segment because the row was deleted.
        let (field, column) = crate::bridge::variant_column("variant", &maps);
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

    crate::snapshot::parquet_io::write_batch(&batch, out)?;
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

/// Replace by rename: a reader either sees the old file or the new one.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
