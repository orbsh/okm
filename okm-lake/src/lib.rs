//! `okm-lake` — project the change log into Parquet files (ADR-0056 §8).
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

pub mod bounds;
pub mod bridge;
pub mod delta;
pub mod read;
pub mod snapshot;
pub mod status;
pub mod vacuum;

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
        write!(f, "okm-lake: {}", self.what)
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
    /// The largest number of rows one file may hold. `0` (the default) is "however many the
    /// window holds": a window becomes one file per partition value. A limit splits it into
    /// several, which is what keeps a big window readable — and what makes the key range in
    /// each manifest narrower than the window.
    pub max_rows_per_file: usize,
    /// Partition the exported table by this KEY column: one directory per value
    /// (`data/<column>=<value>/`) and one file per value per window. `None` (the default)
    /// writes one file per window into `data/`.
    pub partition: Option<String>,
    /// Fold a table into one current-state file when its change files pile up. `None` (the
    /// default) leaves the table alone: a caller that wants current state on demand runs
    /// `okm-lake condense` instead.
    pub fold: Option<FoldPolicy>,
}

impl DrainConfig {
    pub fn new(out_dir: impl Into<PathBuf>, consumer: impl Into<String>) -> Self {
        Self {
            out_dir: out_dir.into(),
            consumer: consumer.into(),
            max_segments: 16,
            max_rows_per_file: 0,
            partition: None,
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
    /// Tables the policy found nothing to do for: every key already lived in one file and
    /// nothing was removed, so a fold would only churn the log. The table is untouched and
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
    /// One `(column, min hex, max hex)` per prunable key column: the range this file covers.
    /// A fold uses it to skip files that cannot share a key with the ones it must resolve
    /// (`#[serde(default)]`: manifests written before bounds existed prune nothing, which is
    /// safe — they simply stay fold candidates).
    #[serde(default)]
    pub key_bounds: Vec<(String, String, String)>,
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
    match crate::read::condense_at(&table_dir, cfg.max_rows_per_file) {
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

/// One window: its files, their manifests, the position, and the Delta commit.
///
/// A partitioned table writes one file per partition VALUE into `data/<column>=<value>/`, and
/// the value travels into the Delta action as a string. Everything else — the watermark, the
/// half-commit rule, the manifests — is per window and does not care how many files it took.
fn drain_one(cfg: &DrainConfig, report: &mut Report, win: &Window) -> Result<()> {
    let first = window::stem_of(&win.segments[0]);
    let last = window::stem_of(&win.segments[win.segments.len() - 1]);
    // One directory per table, `data/` inside it: that directory IS the exported
    // table (ADR-0056 §8), and a reader is pointed at it rather than at a pile of
    // files that also serves other tables.
    let table_dir = cfg.out_dir.join(&win.table);
    let data_dir = table_dir.join("data");

    let key_names: Vec<String> = win
        .schema
        .key_fields
        .iter()
        .map(|f| f.name.clone())
        .collect();
    // A partition must refine the key space: partition by anything else and one key could
    // live in two partitions, where a fold can no longer see both halves of it.
    let partition = match cfg.partition.as_deref() {
        Some(column) => {
            if !key_names.iter().any(|key| key == column) {
                return Err(DrainError::new(format!(
                    "`{column}` is not a key column of this table: a partition has to refine the \
                     key space, and {key_names:?} is that space"
                )));
            }
            Some(column)
        }
        None => None,
    };

    let (batch, whole) = project(win)?;
    let groups = split_by_partition(&batch, partition)?;

    // Written into a scratch name beside the watermark and moved into place only once the
    // projection succeeded: a refused window must leave no directory tree, no file and no
    // moved position behind, which is the half-commit rule the tests hold this to.
    std::fs::create_dir_all(&data_dir)?;
    let mut written: Vec<PathBuf> = Vec::new();
    for (at, (values, chunk)) in groups.iter().enumerate() {
        // A partition this window did not touch gets no file: an empty parquet would be a
        // window that says nothing, and a reader would have to open it to find that out.
        if chunk.num_rows() == 0 {
            continue;
        }
        let dir = match values.first() {
            Some((column, value)) => data_dir.join(format!("{column}={}", partition_dir(value))),
            None => data_dir.clone(),
        };
        std::fs::create_dir_all(&dir)?;
        // A window larger than the limit becomes several files. Their names carry the part
        // number; a window that fits keeps the plain name it always had.
        let parts = split_rows(chunk, cfg.max_rows_per_file)?;
        for (part, rows) in parts.iter().enumerate() {
            let suffix = if parts.len() == 1 {
                String::new()
            } else {
                format!("-p{part}")
            };
            let name = format!("{}-{first}-{last}{suffix}.parquet", win.table);
            let out = dir.join(&name);
            let scratch = cfg.out_dir.join(format!(
                ".{}-{first}-{last}-{at}-{part}.parquet.partial",
                win.table
            ));
            crate::snapshot::parquet_io::write_batch(rows, &scratch)?;
            std::fs::rename(&scratch, &out)?;

            // The manifest lands with the file: a sink records provenance from it without
            // opening the Parquet, and an operator answers "did window N land".
            let described = describe(rows, &key_names)?;
            let manifest = WindowManifest {
                table: win.table.clone(),
                layout_version: win.layout_version,
                segments: win.segments.clone(),
                rows: described.rows,
                deletes: described.deletes,
                columns: described.columns,
                file: window::file_name(&out),
                key_bounds: described.key_bounds,
            };
            write_atomic(
                &out.with_extension("manifest.json"),
                &serde_json::to_string_pretty(&manifest)?,
            )?;
            written.push(out);
        }
    }

    // Only now: the files that cover the window exist, and so do their manifests.
    write_watermark(
        &cfg.out_dir,
        &cfg.consumer,
        &win.table,
        &format!("{last}.segment"),
    )?;

    // The Delta view: this window is one more `add` per file in the log, not a rebuild of
    // everything before it. Committed after the position moves, because a log entry
    // describes a file that exists. Every file under `data/` is offered so a deleted or
    // never-written log heals itself; the ones already recorded are skipped.
    let files = scan_data(&data_dir)?;
    // The key columns travel into the log's schema metadata (`okm.key`): a reader that
    // folds history needs the table's identity, and a file cannot say which of its columns
    // that is (ADR-0058 § Amendment).
    delta::commit_files(&table_dir, &files, &key_names)
        .map_err(|e| DrainError::new(format!("delta: {e}")))?;
    report.metadata += 1;

    report.segments += win.segments.len();
    report.rows += win.rows.len();
    report.deletes += whole.deletes;
    report.derived_deletes += whole.derived_deletes;
    report.open_fields += win.dynamic.len();
    report.files.extend(written);
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
    /// `(column, min hex, max hex)` per prunable key column: what the file covers, so a fold
    /// can skip it (`bounds`).
    pub key_bounds: Vec<(String, String, String)>,
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
/// Build the window's batch — declared columns, the `variant` column, the change columns —
/// without writing anything. The caller decides where its rows go: one file, or one file per
/// partition value.
pub fn project(win: &Window) -> Result<(arrow::array::RecordBatch, Projection)> {
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
    // Two dynamic segments can name the same key (two writes in one window). Their open
    // halves MERGE in log order — the later field wins, and fields the earlier one wrote
    // that the later one did not stay — instead of the last map replacing the first.
    let mut by_key: std::collections::HashMap<&[u8], okm_dynamic::ValueMap> =
        std::collections::HashMap::new();
    for (key, map) in &win.dynamic {
        match by_key.get_mut(key.as_slice()) {
            Some(kept) => crate::read::merge_open(kept, map),
            None => {
                by_key.insert(key.as_slice(), map.clone());
            }
        }
    }

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
                variant_of.push(by_key.get(key.as_slice()));
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

    // Where the file's keys start and end, before the batch goes anywhere: the fold prunes
    // with this, and a manifest that has to be re-read from disk to know it would be a
    // manifest that arrived too late.
    let key_names: Vec<String> = win
        .schema
        .key_fields
        .iter()
        .map(|f| f.name.clone())
        .collect();
    projection.key_bounds = crate::bounds::to_manifest(
        &crate::bounds::of_batch(&batch, &key_names).map_err(DrainError::new)?,
    );

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
    Ok((batch, projection))
}

/// Describe a slice of a projected batch: what a file written from it holds.
fn describe(batch: &arrow::array::RecordBatch, key_names: &[String]) -> Result<Projection> {
    let ops = batch
        .column_by_name("okm_op")
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>());
    let mut projection = Projection {
        columns: batch
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
            .collect(),
        rows: batch.num_rows(),
        deletes: 0,
        open_only: 0,
        derived_deletes: 0,
        key_bounds: crate::bounds::to_manifest(
            &crate::bounds::of_batch(batch, key_names).map_err(DrainError::new)?,
        ),
    };
    if let Some(ops) = ops {
        for row in 0..batch.num_rows() {
            match ops.value(row) {
                OKM_DEL => projection.deletes += 1,
                OKM_PUT_OPEN => projection.open_only += 1,
                _ => {}
            }
        }
    }
    Ok(projection)
}

/// Split a projected batch by one key column's value: one batch per value, in first-seen
/// order, each with the partition values a Delta `add` has to carry.
fn split_by_partition(
    batch: &arrow::array::RecordBatch,
    column: Option<&str>,
) -> Result<Vec<(delta::Partition, arrow::array::RecordBatch)>> {
    let Some(column) = column else {
        return Ok(vec![(Vec::new(), batch.clone())]);
    };
    let values = batch
        .column_by_name(column)
        .ok_or_else(|| DrainError::new(format!("no column `{column}` to partition by")))?;
    let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
    for row in 0..batch.num_rows() {
        let text = partition_text(values.as_ref(), row).ok_or_else(|| {
            DrainError::new(format!(
                "`{column}` is null in row {row}: a key column cannot be, and a partition \
                 value cannot be missing"
            ))
        })?;
        match groups.iter_mut().find(|(kept, _)| *kept == text) {
            Some((_, rows)) => rows.push(row as u32),
            None => groups.push((text, vec![row as u32])),
        }
    }
    let mut out = Vec::with_capacity(groups.len());
    for (text, rows) in groups {
        let indices = arrow::array::UInt32Array::from(rows);
        let columns = batch
            .columns()
            .iter()
            .map(|c| arrow::compute::take(c.as_ref(), &indices, None))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| DrainError::new(format!("partition split: {e}")))?;
        let chunk = arrow::array::RecordBatch::try_new(batch.schema(), columns)
            .map_err(|e| DrainError::new(format!("partition split: {e}")))?;
        out.push((vec![(column.to_string(), text)], chunk));
    }
    Ok(out)
}

/// Cut a batch into runs of at most `max` rows. A `max` of 0 means "keep it whole": the
/// limit is a reader's convenience, not a promise about the data.
pub(crate) fn split_rows(
    batch: &arrow::array::RecordBatch,
    max: usize,
) -> Result<Vec<arrow::array::RecordBatch>> {
    if max == 0 || batch.num_rows() <= max {
        return Ok(vec![batch.clone()]);
    }
    let mut out = Vec::with_capacity(batch.num_rows().div_ceil(max));
    let mut at = 0;
    while at < batch.num_rows() {
        let rows = (at + max).min(batch.num_rows()) - at;
        let indices =
            arrow::array::UInt32Array::from((at as u32..(at + rows) as u32).collect::<Vec<_>>());
        let columns = batch
            .columns()
            .iter()
            .map(|c| arrow::compute::take(c.as_ref(), &indices, None))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| DrainError::new(format!("row split: {e}")))?;
        out.push(
            arrow::array::RecordBatch::try_new(batch.schema(), columns)
                .map_err(|e| DrainError::new(format!("row split: {e}")))?,
        );
        at += rows;
    }
    Ok(out)
}

/// A partition value as text — the form Delta stores, not the form a path uses.
fn partition_text(column: &dyn arrow::array::Array, row: usize) -> Option<String> {
    use arrow::array::*;
    if column.is_null(row) {
        return None;
    }
    macro_rules! number {
        ($ty:ty) => {{
            let values = column.as_any().downcast_ref::<$ty>()?;
            Some(values.value(row).to_string())
        }};
    }
    match column.data_type() {
        arrow::datatypes::DataType::UInt8 => number!(UInt8Array),
        arrow::datatypes::DataType::UInt16 => number!(UInt16Array),
        arrow::datatypes::DataType::UInt32 => number!(UInt32Array),
        arrow::datatypes::DataType::UInt64 => number!(UInt64Array),
        arrow::datatypes::DataType::Int64 => number!(Int64Array),
        arrow::datatypes::DataType::Utf8 => {
            let values = column.as_any().downcast_ref::<StringArray>()?;
            Some(values.value(row).to_string())
        }
        arrow::datatypes::DataType::Boolean => {
            let values = column.as_any().downcast_ref::<BooleanArray>()?;
            Some(values.value(row).to_string())
        }
        arrow::datatypes::DataType::Binary => {
            let values = column.as_any().downcast_ref::<BinaryArray>()?;
            Some(crate::bounds::to_hex(values.value(row)))
        }
        arrow::datatypes::DataType::FixedSizeBinary(_) => {
            let values = column.as_any().downcast_ref::<FixedSizeBinaryArray>()?;
            Some(crate::bounds::to_hex(values.value(row)))
        }
        _ => None,
    }
}

/// The directory name a partition value takes: anything outside `[A-Za-z0-9._-]` is
/// percent-encoded, because a value is data and a path is not a place for `/` or `=`.
pub(crate) fn partition_dir(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The inverse, for reading a partition value back out of a directory name.
pub(crate) fn partition_value(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let text = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(text, 16) {
                out.push(byte);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every partition directory under `data/`, so a commit can name the file and its value.
fn scan_data(data_dir: &Path) -> Result<Vec<delta::Addition>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(data_dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let (column, value) = match name.split_once('=') {
                Some((column, value)) => (column.to_string(), partition_value(value)),
                None => continue,
            };
            for file in std::fs::read_dir(&path)? {
                let file = file?.path();
                if file.extension().is_some_and(|e| e == "parquet") {
                    out.push(delta::Addition {
                        path: file,
                        partition: vec![(column.clone(), value.clone())],
                    });
                }
            }
        } else if path.extension().is_some_and(|e| e == "parquet") {
            out.push(delta::Addition::plain(path));
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Replace by rename: a reader either sees the old file or the new one.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
