//! Read an exported table back, and fold history into current state.
//!
//! The drain writes CHANGE files (ADR-0058 § Amendment): every row carries `okm_op`,
//! `okm_slot` and `okm_seq`, and the Delta log says which file was added by which version.
//! That is enough to answer the two questions a reader has without any of okm's types:
//!
//! * what does this table say NOW — [`current_state`], which applies the ops in order, keeps
//!   the last one per key, and MERGES the open fields a key wrote across windows;
//! * hand me that as files — [`condense`], which rewrites the table to one current-state
//!   file and commits the removal of the ones it replaced.
//!
//! Nothing here decodes a document: the columns are the declaration's, the key is marked in
//! the log's schema metadata (`okm.key`), and a fold is a filter plus a `take` over Arrow
//! batches. Folding is the sink-side half of the pair ADR-0058 names — expressing a removal
//! as table STATE (a Delta `remove` action over whole files) rather than as a change row.

use arrow::array::{
    Array, ArrayRef, BinaryArray, RecordBatch, StringArray, StructArray, UInt32Array,
};
use arrow::compute::{concat_batches, take};
use arrow::row::{RowConverter, SortField};
use okm_value::{DynamicValue, ValueMap};
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
        write!(f, "okm-lake: {}", self.what)
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
    let schema = delta::schema_string_at(table_dir)?
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
    let files = delta::files_in_order_at(table_dir)?;
    let (schema, batches) = aligned_files(table_dir, &files)?;
    let keys = key_columns(table_dir)?;
    resolve(&schema, &batches, &keys)
}

/// Fold a run of batches (already aligned to one schema) into current state.
///
/// Works on whatever run of files the caller hands it: the whole table for
/// [`current_state`], or only the files a fold has to rewrite.
fn resolve(
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
        /// The document's open half, accumulated across every window that wrote one.
        open: Option<ValueMap>,
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
                });
            entry.last_op = op.to_string();
            if op == crate::OKM_PUT {
                entry.put = Some(at);
                entry.order = at;
            }
            if let Some(cell) = opens
                .as_ref()
                .and_then(|c| c.as_any().downcast_ref::<StructArray>())
                && !cell.is_null(row)
            {
                let half = open_fields(cell, row)?;
                match entry.open.as_mut() {
                    Some(kept) => merge_open(kept, &half),
                    None => entry.open = Some(half),
                }
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

    let rows = live.len();

    let joined = concat_batches(schema, batches)?;
    // The declared side comes from the row that last wrote a full row; the open side is
    // re-encoded from the map below, because it may be a half no single row holds.
    let declared = UInt32Array::from(
        live.iter()
            .map(|(_, w)| w.put.unwrap_or(w.order) as u32)
            .collect::<Vec<_>>(),
    );
    let (variant_at, okm_op_at, okm_slot_at, okm_seq_at) = (
        schema.index_of(OPEN_FIELDS_COLUMN).ok(),
        schema.index_of("okm_op").ok(),
        schema.index_of("okm_slot").ok(),
        schema.index_of("okm_seq").ok(),
    );
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for column in joined.columns() {
        arrays.push(take(column.as_ref(), &declared, None)?);
    }
    if let Some(at) = variant_at {
        let maps: Vec<Option<&ValueMap>> = live.iter().map(|(_, w)| w.open.as_ref()).collect();
        let (_, column) = crate::bridge::variant_column(OPEN_FIELDS_COLUMN, &maps);
        arrays[at] = column;
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
///
/// Deciding this costs a look at the manifests, not at the table: each one records the range
/// its keys cover, so two files can only share a key when those ranges overlap. Files that
/// cannot overlap anything are never opened; only the overlapping ones have their key columns
/// read, and that read confirms what the ranges only guessed.
fn files_to_fold(
    table_dir: &Path,
    files: &[String],
    keys: &[String],
) -> Result<(Vec<String>, usize)> {
    use crate::bounds::Bounds;

    // Ranges and removals from the manifests: no data file is opened here.
    let mut bounds: Vec<Option<Bounds>> = Vec::with_capacity(files.len());
    let mut removes = vec![false; files.len()];
    for (at, rel) in files.iter().enumerate() {
        let manifest = crate::status::manifest_of(&table_dir.join(rel));
        removes[at] = manifest.as_ref().is_some_and(|m| m.deletes > 0);
        bounds.push(
            manifest
                .map(|m| crate::bounds::from_manifest(&m.key_bounds))
                .filter(|b| !b.is_empty()),
        );
    }

    // A file is a candidate when it removes something, when nothing bounds it, or when its
    // range overlaps another's. Overlap on ONE key column is a necessary condition for sharing
    // a key, so sweeping that column finds every candidate pair in a sorted pass.
    let mut candidate = removes.clone();
    let first = keys.first().cloned().unwrap_or_default();
    // One column's ranges, in sortable order: overlap there is a necessary condition for
    // sharing a key, so this pass finds every pair a key read would confirm.
    let mut ranged: Vec<(usize, Vec<u8>, Vec<u8>)> = Vec::new();
    for at in 0..files.len() {
        match bounds[at].as_ref().and_then(|b| b.on(&first)) {
            Some(bound) => ranged.push((at, bound.min.clone(), bound.max.clone())),
            None => candidate[at] = true,
        }
    }
    ranged.sort_by(|left, right| left.1.cmp(&right.1));
    let mut reach: Option<(usize, Vec<u8>)> = None;
    for (at, min, max) in ranged {
        match reach.take() {
            None => reach = Some((at, max)),
            Some((prev, widest)) => {
                if min <= widest {
                    candidate[at] = true;
                    candidate[prev] = true;
                }
                reach = Some(if max > widest {
                    (at, max)
                } else {
                    (prev, widest)
                });
            }
        }
    }

    // Only the candidates have their keys read, and only they can share one.
    let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut touched = vec![false; files.len()];
    for (at, rel) in files.iter().enumerate() {
        if !candidate[at] {
            continue;
        }
        let (here, removed) = keys_of_file(&table_dir.join(rel), keys)?;
        if removed {
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
    // Files the ranges excluded are not "untouched by luck": they were never opened, and
    // saying so is what makes the pruning visible from outside.
    let pruned = candidate.iter().filter(|c| !**c).count();
    Ok((
        files
            .iter()
            .zip(touched)
            .filter(|(_, touched)| *touched)
            .map(|(rel, _)| rel.clone())
            .collect(),
        pruned,
    ))
}

/// One row's open half, decoded from its `variant` cell: the object a document's open fields
/// live in. Public because a reader of the export needs it as much as a fold does.
///
/// The cell is `struct(metadata, value)` in the Parquet variant encoding, and the cell's object
/// is a document's open fields. Reading it is what lets a fold MERGE two windows' open halves
/// instead of dropping the older one.
pub fn open_fields(cell: &StructArray, row: usize) -> Result<ValueMap> {
    let bytes = |at: usize| -> Result<&[u8]> {
        let column = cell
            .column(at)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or_else(|| ReadError::new("a variant cell is not (metadata, value) bytes"))?;
        Ok(column.value(row))
    };
    let variant = parquet_variant::Variant::try_new(bytes(0)?, bytes(1)?)
        .map_err(|e| ReadError::new(format!("variant cell: {e}")))?;
    let object = variant
        .as_object()
        .ok_or_else(|| ReadError::new("a variant cell holds no object"))?;
    let mut map = ValueMap::new();
    for (name, value) in object.iter() {
        map.insert(name.to_string(), as_dynamic(&value)?);
    }
    Ok(map)
}

/// A variant value as the dynamic vocabulary. The widths are the variant's, not the
/// declaration's: a column's type is the declaration's business, and a fold only has to
/// carry the value through.
fn as_dynamic(value: &parquet_variant::Variant<'_, '_>) -> Result<DynamicValue> {
    if value.as_null().is_some() {
        return Ok(DynamicValue::Null);
    }
    if let Some(x) = value.as_boolean() {
        return Ok(DynamicValue::Bool(x));
    }
    if let Some(x) = value.as_string() {
        return Ok(DynamicValue::Str(x.to_string()));
    }
    if let Some(x) = value.as_u8_slice() {
        return Ok(DynamicValue::Bytes(x.to_vec()));
    }
    if let Some(x) = value.as_int64() {
        return Ok(DynamicValue::I64(x));
    }
    if let Some(x) = value.as_u64() {
        return Ok(DynamicValue::U64(x));
    }
    if let Some(x) = value.as_f64() {
        return Ok(DynamicValue::F64(x));
    }
    if let Some(list) = value.as_list() {
        let mut items = Vec::with_capacity(list.len());
        for item in list.iter() {
            items.push(as_dynamic(&item)?);
        }
        return Ok(DynamicValue::Array(items));
    }
    if let Some(object) = value.as_object() {
        let mut map = ValueMap::new();
        for (name, value) in object.iter() {
            map.insert(name.to_string(), as_dynamic(&value)?);
        }
        return Ok(DynamicValue::Obj(map));
    }
    Err(ReadError::new(
        "a variant cell holds a value outside the dynamic vocabulary (date, decimal, uuid…)",
    ))
}

/// `source` over `target`: a field the later window wrote wins, and two objects merge field by
/// field so a document's open half accumulates instead of being replaced wholesale.
pub(crate) fn merge_open(target: &mut ValueMap, source: &ValueMap) {
    for (name, value) in source {
        let nested = match (target.get(name), value) {
            (Some(DynamicValue::Obj(kept)), DynamicValue::Obj(next)) => {
                let mut merged = kept.clone();
                merge_open(&mut merged, next);
                Some(DynamicValue::Obj(merged))
            }
            _ => None,
        };
        match nested {
            Some(merged) => {
                target.insert(name.clone(), merged);
            }
            None => {
                target.insert(name.clone(), value.clone());
            }
        }
    }
}

/// What one fold did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldReport {
    /// The version the fold committed.
    pub version: u64,
    /// The files that now hold the table's current state: one per partition, or one when the
    /// table is not partitioned.
    pub files: Vec<PathBuf>,
    /// How many files it replaced.
    pub replaced: usize,
    /// How many files it left alone because they already state final rows.
    pub untouched: usize,
    /// How many files the key ranges excluded without opening them at all.
    pub pruned: usize,
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
    condense_at(table_dir, 0)
}

/// Fold the table, with at most `max_rows` rows per file (`0` = one file per partition).
pub fn condense_at(table_dir: &Path, max_rows: usize) -> Result<FoldReport> {
    let files = delta::files_in_order_at(table_dir)?;
    let keys = key_columns(table_dir)?;
    let (touched, pruned) = files_to_fold(table_dir, &files, &keys)?;
    if touched.is_empty() {
        return Err(ReadError::refused(format!(
            "{}: every key lives in one file and nothing is removed, nothing to fold",
            table_dir.display()
        )));
    }
    let untouched = files.len() - touched.len();

    let hex = table_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ReadError::new(format!("{}: not a table directory", table_dir.display())))?
        .to_string();
    let version = delta::next_version_at(table_dir)?;
    let data_dir = table_dir.join("data");
    std::fs::create_dir_all(&data_dir)?;

    // A fold stays INSIDE the partition it resolves: a table is partitioned by a KEY column,
    // so the same key cannot be in two partitions, and a fold that crossed them would write
    // one file whose partitionValues describe two different values.
    let live = delta::live_adds_at(table_dir)?;
    let mut groups: Vec<(delta::Partition, Vec<String>)> = Vec::new();
    for rel in &touched {
        let values = live
            .iter()
            .find(|(path, _)| path == rel)
            .map(|(_, values)| values.clone())
            .unwrap_or_default();
        match groups.iter_mut().find(|(kept, _)| *kept == values) {
            Some((_, files)) => files.push(rel.clone()),
            None => groups.push((values, vec![rel.clone()])),
        }
    }

    let stem = format!("{hex}-fold-{version:020}");
    let mut additions: Vec<delta::Addition> = Vec::new();
    let mut written: Vec<PathBuf> = Vec::new();
    let mut rows = 0usize;
    for (at, (values, files)) in groups.iter().enumerate() {
        let (schema, batches) = aligned_files(table_dir, files)?;
        let (batch, _) = resolve(&schema, &batches, &keys)?;
        // Nothing left to say in this partition: its files go, and no file replaces them.
        if batch.num_rows() == 0 {
            continue;
        }
        let dir = match values.first() {
            Some((column, value)) => {
                data_dir.join(format!("{column}={}", crate::partition_dir(value)))
            }
            None => data_dir.clone(),
        };
        std::fs::create_dir_all(&dir)?;
        // A folded partition larger than the limit becomes several files, the same way a
        // window does — and each part's manifest states a narrower key range than the whole.
        let parts = crate::split_rows(&batch, max_rows).map_err(|e| ReadError::new(e.what))?;
        for (part, chunk) in parts.iter().enumerate() {
            let suffix = if parts.len() == 1 {
                String::new()
            } else {
                format!("-p{part}")
            };
            let name = format!("{stem}{suffix}.parquet");
            let out = dir.join(&name);
            let scratch = table_dir.join(format!(".{at}-{part}-{name}.partial"));
            crate::snapshot::parquet_io::write_batch(chunk, &scratch)?;
            std::fs::rename(&scratch, &out)?;

            // A folded file gets a manifest too: it carries the segments the files it
            // replaced described, so provenance survives a fold and `status` can still
            // count rows.
            let mut segments: Vec<String> = Vec::new();
            let mut layout_version = 0u8;
            for rel in files {
                if let Some(manifest) = crate::status::manifest_of(&table_dir.join(rel)) {
                    layout_version = layout_version.max(manifest.layout_version);
                    for segment in manifest.segments {
                        if !segments.contains(&segment) {
                            segments.push(segment);
                        }
                    }
                }
            }
            let manifest = crate::WindowManifest {
                table: hex.clone(),
                layout_version,
                segments,
                rows: chunk.num_rows(),
                deletes: 0,
                columns: chunk
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
                file: name.clone(),
                key_bounds: crate::bounds::to_manifest(
                    &crate::bounds::of_batch(chunk, &keys).map_err(ReadError::new)?,
                ),
            };
            std::fs::write(
                out.with_extension("manifest.json"),
                serde_json::to_string_pretty(&manifest)
                    .map_err(|e| ReadError::new(format!("manifest: {e}")))?,
            )?;

            rows += chunk.num_rows();
            let relative = out
                .strip_prefix(table_dir)
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .map_err(|_| {
                    ReadError::new(format!(
                        "{}: not inside {}",
                        out.display(),
                        table_dir.display()
                    ))
                })?;
            let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            additions.push(delta::Addition {
                path: relative,
                partition: values.clone(),
                size,
            });
            written.push(out);
        }
    }

    let commit = delta::commit_compaction_at(table_dir, &touched, &additions, &keys)?;
    let files = written;
    Ok(FoldReport {
        version: commit.version,
        files,
        replaced: touched.len(),
        untouched,
        pruned,
        rows,
    })
}
