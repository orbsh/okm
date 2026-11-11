//! Read an exported table back, and fold history into current state.
//!
//! The drain writes CHANGE files (ADR-0058 § Amendment): every row carries `okm_op`,
//! `okm_slot` and `okm_seq`, and the Delta log says which file was added by which version.
//! That is enough to answer the two questions a reader has without any of okm's types:
//!
//! * what does this table say NOW — [`current_state`], which applies the ops in order and
//!   keeps the last one per key;
//! * hand me that as files — [`condense`], which rewrites the table to one current-state
//!   file and commits the removal of the ones it replaced.
//!
//! Nothing here decodes a document: the columns are the declaration's, the key is marked in
//! the log's schema metadata (`okm.key`), and a fold is a filter plus a `take` over Arrow
//! batches. Folding is the sink-side half of the pair ADR-0058 names — expressing a removal
//! as table STATE (a Delta `remove` action over whole files) rather than as a change row.

use arrow::array::{Array, ArrayRef, RecordBatch, StringArray, StructArray, UInt32Array};
use arrow::compute::{concat_batches, take};
use arrow::row::{RowConverter, SortField};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::delta;

/// What a failed read or fold says. Same rules as the drain's error: the caller's decision
/// is the same for io, Parquet, schema and op problems — stop, fix, run again.
#[derive(Debug)]
pub struct ReadError {
    pub what: String,
    /// True when the TABLE is fine and this FOLD is not — the answer a caller that folds on
    /// a policy needs, because a table that cannot be folded must not stop the drain.
    pub refused: bool,
}

impl ReadError {
    fn new(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            refused: false,
        }
    }

    fn refused(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            refused: true,
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "okm-parquet: {}", self.what)
    }
}

impl std::error::Error for ReadError {}

impl From<std::io::Error> for ReadError {
    fn from(e: std::io::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<parquet::errors::ParquetError> for ReadError {
    fn from(e: parquet::errors::ParquetError) -> Self {
        Self::new(format!("parquet: {e}"))
    }
}

impl From<arrow::error::ArrowError> for ReadError {
    fn from(e: arrow::error::ArrowError) -> Self {
        Self::new(format!("arrow: {e}"))
    }
}

impl From<String> for ReadError {
    fn from(e: String) -> Self {
        Self::new(e)
    }
}

pub type Result<T> = std::result::Result<T, ReadError>;

/// The op names a change row carries (ADR-0058 § Amendment). Re-exported from the writer's
/// constants so a reader and a writer cannot drift apart.
pub use crate::{OKM_DEL, OKM_PUT, OKM_PUT_OPEN};

/// The export's open-fields column: one `variant` cell per row (ADR-0045 §4).
const OPEN_FIELDS_COLUMN: &str = "variant";

/// Which columns are the table's key, in the order the log's schema states them.
///
/// Read from the log, not guessed: `okm.key` in the schema metadata is the one thing a file
/// cannot say about itself, and an export written before the amendment has none — which is a
/// refusal here rather than a fold on a made-up identity.
pub fn key_columns(table_dir: &Path) -> Result<Vec<String>> {
    let schema = delta::schema_string(table_dir)?
        .ok_or_else(|| ReadError::new("this table has no schema in its log yet"))?;
    let parsed: serde_json::Value = serde_json::from_str(&schema)
        .map_err(|e| ReadError::new(format!("schema string is not JSON: {e}")))?;
    let mut keys = Vec::new();
    for field in parsed["fields"]
        .as_array()
        .ok_or_else(|| ReadError::new("schema string has no fields"))?
    {
        let marked = field["metadata"]["okm.key"].as_str() == Some("true");
        if marked && let Some(name) = field["name"].as_str() {
            keys.push(name.to_string());
        }
    }
    if keys.is_empty() {
        return Err(ReadError::new(
            "the log's schema marks no key columns — an export written before ADR-0058's \
             amendment cannot be folded, and its rows have no identity to fold on",
        ));
    }
    Ok(keys)
}

/// The schema the table's files agree on: every field any of them carries, in first-seen
/// order. A file that lacks one (the `variant` column of windows with no open fields) reads
/// as NULL for it rather than failing the fold.
fn union_schema(batches: &[RecordBatch]) -> Result<arrow::datatypes::SchemaRef> {
    let mut fields: Vec<arrow::datatypes::Field> = Vec::new();
    for batch in batches {
        for field in batch.schema().fields() {
            match fields.iter().find(|f| f.name() == field.name()) {
                Some(known) if known.data_type() == field.data_type() => {}
                Some(known) => {
                    return Err(ReadError::new(format!(
                        "column `{}` is {} in one file and {} in another",
                        field.name(),
                        known.data_type(),
                        field.data_type()
                    )));
                }
                None => fields.push(field.as_ref().clone()),
            }
        }
    }
    Ok(std::sync::Arc::new(arrow::datatypes::Schema::new(fields)))
}

/// One batch against the union schema, with NULLs for the columns it does not have.
fn aligned(batch: &RecordBatch, schema: &arrow::datatypes::SchemaRef) -> Result<RecordBatch> {
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        match batch.column_by_name(field.name()) {
            Some(column) => arrays.push(column.clone()),
            None => arrays.push(arrow::array::new_null_array(
                field.data_type(),
                batch.num_rows(),
            )),
        }
    }
    Ok(RecordBatch::try_new(schema.clone(), arrays)?)
}

/// The given data files, in the order given, one batch list per file.
fn read_files(table_dir: &Path, files: &[String]) -> Result<Vec<RecordBatch>> {
    if files.is_empty() {
        return Err(ReadError::new("this table has no data files"));
    }
    let mut batches = Vec::new();
    for rel in files {
        let path = table_dir.join(rel);
        let file = std::fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?;
        for batch in reader.build()? {
            batches.push(batch?);
        }
    }
    Ok(batches)
}

/// The files' batches against the schema the table agrees on.
fn aligned_files(
    table_dir: &Path,
    files: &[String],
) -> Result<(arrow::datatypes::SchemaRef, Vec<RecordBatch>)> {
    let raw = read_files(table_dir, files)?;
    // Files of one table do not have to be column-for-column identical: a window with no
    // open fields has no `variant` column. The table's schema is the union, and a file that
    // lacks a column reads as NULL in it.
    let schema = union_schema(&raw)?;
    let batches = raw
        .iter()
        .map(|b| aligned(b, &schema))
        .collect::<Result<_>>()?;
    Ok((schema, batches))
}

/// The table as of its newest version: one row per key that was put and not removed, with
/// `okm_seq` renumbered (it means "position in this file", and after folding there is one).
///
/// Order inside a file is `okm_seq`; order ACROSS files is the Delta version that added them,
/// which is what `files_in_order` reads. A key whose last op is a `del` is simply absent —
/// that is what current state means, and the change history is still in the log for anyone
/// who asks the question the other way round.
pub fn current_state(table_dir: &Path) -> Result<RecordBatch> {
    Ok(state_and_total(table_dir)?.0)
}

/// The same answer, plus how many change rows the files hold — the number a fold compares
/// itself against to decide whether there is anything to fold.
fn state_and_total(table_dir: &Path) -> Result<(RecordBatch, usize)> {
    let files = delta::files_in_order(table_dir)?;
    let (schema, batches) = aligned_files(table_dir, &files)?;
    let keys = key_columns(table_dir)?;
    resolve(table_dir, &schema, &batches, &keys)
}

/// Fold a run of batches (already aligned to one schema) into current state.
///
/// `label` is only for the refusal's message: a fold of a few files names the table, and the
/// caller that folded them knows which one it is.
fn resolve(
    table_dir: &Path,
    schema: &arrow::datatypes::SchemaRef,
    batches: &[RecordBatch],
    keys: &[String],
) -> Result<(RecordBatch, usize)> {
    // Key columns in FILE order, so two batches are compared the same way.
    let key_fields: Vec<_> = schema
        .fields()
        .iter()
        .filter(|f| keys.iter().any(|k| k == f.name()))
        .map(|f| SortField::new(f.data_type().clone()))
        .collect();
    if key_fields.len() != keys.len() {
        return Err(ReadError::new(format!(
            "the log marks {} key columns but the file has {}: {:?}",
            keys.len(),
            key_fields.len(),
            keys
        )));
    }
    let key_names: Vec<&str> = schema
        .fields()
        .iter()
        .filter(|f| keys.iter().any(|k| k == f.name()))
        .map(|f| f.name().as_str())
        .collect();
    let converter = RowConverter::new(key_fields)?;

    // Per key, which row states what. Walking files in log order and rows in file order
    // means the last row for a key is the newest op: that is what decides whether the key
    // is still there. What it CARRIES is not so simple — a `put_open` row states the open
    // fields and nothing else, so the declared columns come from the last `put` and the open
    // half from the last row that carried any.
    struct Winner {
        last_op: String,
        order: usize,
        put: Option<usize>,
        open: Option<usize>,
        opens: usize,
    }
    let mut newest: HashMap<Vec<u8>, Winner> = HashMap::new();
    let open_at = schema.index_of(OPEN_FIELDS_COLUMN).ok();
    let mut base = 0usize;
    for batch in batches {
        let columns: Vec<ArrayRef> = key_names
            .iter()
            .map(|n| {
                batch
                    .column_by_name(n)
                    .cloned()
                    .ok_or_else(|| ReadError::new(format!("a file has no key column `{n}`")))
            })
            .collect::<Result<_>>()?;
        let rows = converter.convert_columns(&columns)?;
        let ops = batch
            .column_by_name("okm_op")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| ReadError::new("a file has no `okm_op` column"))?;
        let opens = open_at.map(|at| batch.column(at).clone());
        for row in 0..batch.num_rows() {
            let at = base + row;
            let op = ops.value(row);
            let entry = newest
                .entry(rows.row(row).as_ref().to_vec())
                .or_insert(Winner {
                    last_op: String::new(),
                    order: at,
                    put: None,
                    open: None,
                    opens: 0,
                });
            entry.last_op = op.to_string();
            if op == crate::OKM_PUT {
                entry.put = Some(at);
                entry.order = at;
            }
            let carries = opens
                .as_ref()
                .and_then(|c| c.as_any().downcast_ref::<StructArray>())
                .is_some_and(|s| !s.is_null(row));
            if carries {
                entry.open = Some(at);
                entry.opens += 1;
                if entry.put.is_none() {
                    entry.order = at;
                }
            }
        }
        base += batch.num_rows();
    }

    // Live rows, in the order their state was last written.
    let mut live: Vec<(usize, Winner)> = newest
        .into_values()
        .filter(|w| w.last_op != OKM_DEL)
        .map(|w| (w.order, w))
        .collect();
    live.sort_by_key(|(order, _)| *order);

    // A row of open fields is one window's share of a document's open half. Two windows
    // would have to be merged INSIDE the variant blob to state the document's whole open
    // half, and this reader does not decode that blob — so it refuses rather than writing a
    // fold that quietly drops the older window's fields.
    let doubled = live.iter().filter(|(_, w)| w.opens > 1).count();
    let rows = live.len();

    let joined = concat_batches(schema, batches)?;
    // Two index vectors: what the declared side says, and what the open side says.
    let declared: Vec<u32> = live
        .iter()
        .map(|(_, w)| (w.put.unwrap_or(w.order)) as u32)
        .collect();
    let opens: Vec<u32> = live
        .iter()
        .map(|(_, w)| (w.open.unwrap_or(w.put.unwrap_or(w.order))) as u32)
        .collect();
    let declared = UInt32Array::from(declared);
    let opens = UInt32Array::from(opens);
    let (variant_at, okm_op_at, okm_slot_at, okm_seq_at) = (
        schema.index_of(OPEN_FIELDS_COLUMN).ok(),
        schema.index_of("okm_op").ok(),
        schema.index_of("okm_slot").ok(),
        schema.index_of("okm_seq").ok(),
    );
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for (at, column) in joined.columns().iter().enumerate() {
        let picks = if Some(at) == variant_at {
            &opens
        } else {
            &declared
        };
        arrays.push(take(column.as_ref(), picks, None)?);
    }
    if let Some(at) = okm_op_at {
        // A row that outlived the fold is either a full row or open fields only.
        let ops: Vec<&str> = live
            .iter()
            .map(|(_, w)| {
                if w.put.is_some() {
                    crate::OKM_PUT
                } else {
                    crate::OKM_PUT_OPEN
                }
            })
            .collect();
        arrays[at] = std::sync::Arc::new(StringArray::from(ops));
    }
    if let Some(at) = okm_slot_at {
        // No removal survives a fold, so nothing names a face any more.
        arrays[at] = arrow::array::new_null_array(schema.field(at).data_type(), rows);
    }
    if let Some(at) = okm_seq_at {
        // The sequence was a position in ITS file; the folded file has its own.
        arrays[at] = std::sync::Arc::new(UInt32Array::from((0..rows as u32).collect::<Vec<_>>()));
    }
    let batch = RecordBatch::try_new(schema.clone(), arrays)?;
    if doubled > 0 {
        return Err(ReadError::refused(format!(
            "{}: {doubled} key(s) carry open fields written by more than one window, and a \
             fold cannot merge two variant blobs — it would drop the older window's fields",
            table_dir.display()
        )));
    }
    let total = batches.iter().map(RecordBatch::num_rows).sum();
    Ok((batch, total))
}

/// The keys a file holds, and whether it holds a removal — read with a PROJECTION, so the
/// payload is never touched. This is the index a fold needs: a key that appears in one file
/// and was never removed there is already final.
fn keys_of_file(path: &Path, keys: &[String]) -> Result<(Vec<Vec<u8>>, bool)> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let descriptor = builder.metadata().file_metadata().schema_descr().clone();
    let schema = builder.schema().clone();
    let mut roots = Vec::new();
    for (at, field) in schema.fields().iter().enumerate() {
        if field.name() == "okm_op" || keys.iter().any(|k| k == field.name()) {
            roots.push(at);
        }
    }
    if roots.len() != keys.len() + 1 {
        return Err(ReadError::new(format!(
            "{}: a file is missing a key column or `okm_op`",
            path.display()
        )));
    }
    let reader = builder
        .with_projection(parquet::arrow::ProjectionMask::roots(&descriptor, roots))
        .build()?;
    let mut out = Vec::new();
    let mut removed = false;
    let mut converter: Option<(RowConverter, Vec<String>)> = None;
    for batch in reader {
        let batch = batch?;
        let key_names: Vec<String> = batch
            .schema()
            .fields()
            .iter()
            .filter(|f| keys.iter().any(|k| k == f.name()))
            .map(|f| f.name().clone())
            .collect();
        let (conv, names) = match converter.take() {
            Some(ready) => ready,
            None => {
                let fields: Vec<SortField> = batch
                    .schema()
                    .fields()
                    .iter()
                    .filter(|f| keys.iter().any(|k| k == f.name()))
                    .map(|f| SortField::new(f.data_type().clone()))
                    .collect();
                (RowConverter::new(fields)?, key_names.clone())
            }
        };
        let columns: Vec<ArrayRef> = names
            .iter()
            .map(|n| {
                batch
                    .column_by_name(n)
                    .cloned()
                    .ok_or_else(|| ReadError::new(format!("a file has no key column `{n}`")))
            })
            .collect::<Result<_>>()?;
        let rows = conv.convert_columns(&columns)?;
        let ops = batch
            .column_by_name("okm_op")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| ReadError::new("a file has no `okm_op` column"))?;
        for row in 0..batch.num_rows() {
            if ops.value(row) == OKM_DEL {
                removed = true;
            }
            out.push(rows.row(row).as_ref().to_vec());
        }
        converter = Some((conv, key_names));
    }
    Ok((out, removed))
}

/// The files a fold has to rewrite: those holding a removal, and every file whose key also
/// appears in another file. A file whose keys are exclusive to it (and that removes nothing)
/// already states final rows — rewriting it would be work with no answer behind it.
fn files_to_fold(table_dir: &Path, files: &[String], keys: &[String]) -> Result<Vec<String>> {
    let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut touched = vec![false; files.len()];
    for (at, rel) in files.iter().enumerate() {
        let (here, removes) = keys_of_file(&table_dir.join(rel), keys)?;
        if removes {
            touched[at] = true;
        }
        for key in here {
            match seen.entry(key) {
                std::collections::hash_map::Entry::Occupied(first) => {
                    touched[at] = true;
                    touched[*first.get()] = true;
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(at);
                }
            }
        }
    }
    Ok(files
        .iter()
        .zip(touched)
        .filter(|(_, touched)| *touched)
        .map(|(rel, _)| rel.clone())
        .collect())
}

/// What one fold did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldReport {
    /// The version the fold committed.
    pub version: u64,
    /// The file that now holds the table's current state.
    pub file: PathBuf,
    /// How many files it replaced.
    pub replaced: usize,
    /// How many files it left alone because they already state final rows.
    pub untouched: usize,
    /// Rows in that file.
    pub rows: usize,
}

/// Replace the change files that need it with ONE file of current state (ADR-0058 §
/// Amendment: the sink half, stated as a `remove` action over whole files).
///
/// Only files that share a key with another file, or that hold a removal, are rewritten — a
/// file whose keys are exclusive to it already states final rows. That is what makes a fold
/// affordable on a table that keeps growing: the index it needs is the key columns, read with
/// a projection, never the payload.
///
/// A table with nothing to resolve is refused rather than rewritten: folding it would churn
/// the log without changing what a reader sees.
pub fn condense(table_dir: &Path) -> Result<FoldReport> {
    let files = delta::files_in_order(table_dir)?;
    let keys = key_columns(table_dir)?;
    let touched = files_to_fold(table_dir, &files, &keys)?;
    if touched.is_empty() {
        return Err(ReadError::refused(format!(
            "{}: every key lives in one file and nothing is removed, nothing to fold",
            table_dir.display()
        )));
    }
    let (schema, batches) = aligned_files(table_dir, &touched)?;
    let (batch, _) = resolve(table_dir, &schema, &batches, &keys)?;
    let untouched = files.len() - touched.len();

    let hex = table_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ReadError::new(format!("{}: not a table directory", table_dir.display())))?
        .to_string();
    let version = delta::next_version(table_dir)?;
    let data_dir = table_dir.join("data");
    std::fs::create_dir_all(&data_dir)?;
    let name = format!("{hex}-fold-{version:020}.parquet");
    let scratch = table_dir.join(format!(".{name}.partial"));
    crate::snapshot::parquet_io::write_batch(&batch, &scratch)?;
    let out = data_dir.join(&name);
    std::fs::rename(&scratch, &out)?;

    let commit = delta::commit_compaction(table_dir, &touched, &out, &keys)?;
    Ok(FoldReport {
        version: commit.version,
        file: out,
        replaced: touched.len(),
        untouched,
        rows: batch.num_rows(),
    })
}
