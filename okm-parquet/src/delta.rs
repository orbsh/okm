//! The exported directory IS a Delta table (ADR-0056 §8).
//!
//! What a Delta table is, for a writer: a directory holding `data/*.parquet` and a
//! `_delta_log/` of numbered JSON files, each one a list of ACTIONS. The first version
//! declares the protocol and the schema; every later version is a commit. Appending a
//! window is one line (`add`) in one new JSON file, which is why this writer is small
//! enough to read in one sitting — no manifests, no manifest lists, no snapshot files,
//! no Avro encoder, no absolute URIs (an `add` path is relative to the table directory
//! by specification), and no version-hint file to keep in step.
//!
//! Commits are per-file and additive: a window that lands file F writes a version
//! holding `add F` and nothing else, so the log grows by the size of the change rather
//! than by the size of the table.
//!
//! **The variant feature is table-wide.** A `variant` column must be declared as such,
//! and declaring it means `minReaderVersion 3` / `minWriterVersion 7` plus
//! `readerFeatures/writerFeatures: ["variantType"]` — a reader that does not know the
//! feature refuses the whole table. That is the price of a column engines actually
//! DECODE: as a plain binary it is readable everywhere and understood nowhere
//! (measured: the flat form decoded to NULL in every row, the struct form to the value).
//!
//! This lives HERE and not in `okm-core`: the log is derived from the change log, one
//! projection further out than the window. Core produces segments and joins windows;
//! a table format — Parquet, the log, replay — is what this crate does with them
//! (ADR-0056, the "only the formats move out" bullet).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use arrow::datatypes::DataType;
use arrow::datatypes::Field as ArrowField;

/// One commit: which version was written, and what it added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The version number (the file stem), 0 for the first.
    pub version: u64,
    /// The log file this commit wrote.
    pub path: PathBuf,
    /// How many data files this commit added — files already in the log are skipped,
    /// so re-running a drain over the same directory is a no-op version.
    pub added: usize,
}

/// The columns of one data file, in file order, with their Delta type names, plus its
/// row count. One file describes the table: every file a window writes carries the
/// declared schema.
/// What a commit records about one data file: the columns (name + Delta type), the row
/// count, and the per-column bounds a reader can skip on.
#[derive(Debug, Clone, Default)]
pub struct FileSummary {
    pub columns: Vec<(String, String)>,
    pub rows: u64,
    /// `column → min` / `column → max` / `column → nulls`, for the columns whose type
    /// has a JSON form Delta's stats schema accepts (signed integers, floats, strings,
    /// booleans). Binary, variant, struct and UNSIGNED columns are left out: the first
    /// three have no scalar bound, and an unsigned value has no exact Delta type to
    /// state it in.
    pub min: serde_json::Map<String, serde_json::Value>,
    pub max: serde_json::Map<String, serde_json::Value>,
    pub nulls: serde_json::Map<String, serde_json::Value>,
}

/// Which leaf column of the Parquet metadata carries `name`.
///
/// The metadata's column order is not the Arrow schema's: a struct field expands into one
/// leaf per member, so a field after it sits at an offset the Arrow index does not have.
/// A field with no single leaf of its own (struct, list) answers `None` — its type is
/// skipped anyway, and a bound attributed to the wrong column is worse than none.
fn leaf_of(metadata: &parquet::file::metadata::ParquetMetaData, name: &str) -> Option<usize> {
    let first = metadata.row_groups().first()?;
    (0..first.columns().len()).find(|&i| {
        let parts = first.column(i).column_path().parts();
        parts.len() == 1 && parts[0] == name
    })
}

pub fn file_summary(path: &Path) -> Result<FileSummary, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let arrow = reader.schema().clone();
    let mut columns = Vec::with_capacity(arrow.fields().len());
    for f in arrow.fields() {
        let ty = delta_type(f).ok_or_else(|| {
            format!(
                "column `{}` has no Delta type for {:?}",
                f.name(),
                f.data_type()
            )
        })?;
        columns.push((f.name().clone(), ty));
    }
    let metadata = reader.metadata();
    // The row count is the sum over row groups.
    let rows: u64 = metadata
        .row_groups()
        .iter()
        .map(|g| g.num_rows() as u64)
        .sum();

    // Bounds come from the Parquet statistics the writer already computed, folded per
    // column across row groups. A column with no statistics simply gets none here —
    // claiming a bound that was not measured is worse than not skipping.
    let mut summary = FileSummary {
        columns,
        rows,
        ..FileSummary::default()
    };
    let skip_unsigned = |ty: &DataType| {
        matches!(
            ty,
            DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Decimal128(_, _)
                | DataType::Binary
                | DataType::LargeBinary
                | DataType::Struct(_)
                | DataType::List(_)
        )
    };
    for field in arrow.fields().iter() {
        if skip_unsigned(field.data_type()) {
            continue;
        }
        // The Parquet metadata's column order is NOT the Arrow schema's: a struct column
        // (the `variant` half of a document) expands into one leaf column per member, so a
        // field AFTER it would be read from the wrong leaf — a bound and a null count
        // attributed to the wrong name, which a reader then uses to skip the file. Resolve
        // each field to its leaf by PATH instead of trusting the index.
        let Some(leaf) = leaf_of(metadata, field.name()) else {
            continue;
        };
        let mut min: Option<serde_json::Value> = None;
        let mut max: Option<serde_json::Value> = None;
        let mut nulls: u64 = 0;
        let mut counted = true;
        for group in metadata.row_groups() {
            let Some(column) = group.columns().get(leaf) else {
                counted = false;
                break;
            };
            let Some(stats) = column.statistics() else {
                counted = false;
                break;
            };
            match stats.null_count_opt() {
                Some(n) => nulls += n,
                None => {
                    counted = false;
                    break;
                }
            }
            match bounds(stats, field.data_type()) {
                Some((lo, hi)) => {
                    min = Some(match min {
                        None => lo,
                        Some(prev) => pick(prev, lo, true),
                    });
                    max = Some(match max {
                        None => hi,
                        Some(prev) => pick(prev, hi, false),
                    });
                }
                None => {
                    counted = false;
                    break;
                }
            }
        }
        if !counted {
            continue;
        }
        if let Some(lo) = min {
            summary.min.insert(field.name().clone(), lo);
        }
        if let Some(hi) = max {
            summary.max.insert(field.name().clone(), hi);
        }
        summary
            .nulls
            .insert(field.name().clone(), serde_json::Value::from(nulls));
    }
    Ok(summary)
}

/// The smaller (or larger) of two bounds of the same column. Numbers compare numerically,
/// strings lexicographically — which is what the Parquet statistics were built from.
fn pick(a: serde_json::Value, b: serde_json::Value, want_min: bool) -> serde_json::Value {
    let less = match (&a, &b) {
        (serde_json::Value::Number(x), serde_json::Value::Number(y)) => x.as_f64() <= y.as_f64(),
        (serde_json::Value::String(x), serde_json::Value::String(y)) => x <= y,
        (serde_json::Value::Bool(x), serde_json::Value::Bool(y)) => x <= y,
        _ => true,
    };
    if less == want_min { a } else { b }
}

/// One row group's `(min, max)` for a column, as JSON, or `None` when the type has no
/// scalar form here. Strings are cut to the 32 characters Delta's stats schema promises:
/// a longer bound is reported as a prefix, never as a different value.
fn bounds(
    stats: &parquet::file::statistics::Statistics,
    ty: &DataType,
) -> Option<(serde_json::Value, serde_json::Value)> {
    use parquet::file::statistics::Statistics;
    let num = |n: i64| serde_json::Value::from(n);
    let float = |f: f64| serde_json::Number::from_f64(f).map(serde_json::Value::Number);
    match (stats, ty) {
        (Statistics::Boolean(s), DataType::Boolean) => Some((
            serde_json::Value::Bool(*s.min_opt()?),
            serde_json::Value::Bool(*s.max_opt()?),
        )),
        (Statistics::Int32(s), DataType::Int8 | DataType::Int16 | DataType::Int32) => {
            Some((num(*s.min_opt()? as i64), num(*s.max_opt()? as i64)))
        }
        (Statistics::Int64(s), DataType::Int64) => Some((num(*s.min_opt()?), num(*s.max_opt()?))),
        (Statistics::Float(s), DataType::Float32) => {
            Some((float(*s.min_opt()? as f64)?, float(*s.max_opt()? as f64)?))
        }
        (Statistics::Double(s), DataType::Float64) => {
            Some((float(*s.min_opt()?)?, float(*s.max_opt()?)?))
        }
        (Statistics::ByteArray(s), DataType::Utf8) => {
            let lo = std::str::from_utf8(s.min_bytes_opt()?).ok()?;
            let hi = std::str::from_utf8(s.max_bytes_opt()?).ok()?;
            let cut = |v: &str| -> serde_json::Value {
                let head: String = v.chars().take(32).collect();
                serde_json::Value::String(head)
            };
            Some((cut(lo), cut(hi)))
        }
        _ => None,
    }
}

/// Arrow → Delta type name. Unsigned integers have no Delta type of their own, so they
/// WIDEN into the signed type that holds every value (the alternative is a column that
/// cannot be written at all, or a narrowing cast that loses data).
fn delta_type(field: &ArrowField) -> Option<String> {
    let ty = field.data_type();
    let is_variant = field
        .metadata()
        .get("ARROW:extension:name")
        .is_some_and(|n| n == "arrow.parquet.variant");
    if is_variant {
        return Some("variant".to_string());
    }
    Some(match ty {
        DataType::Boolean => "boolean".to_string(),
        DataType::Int8 => "byte".to_string(),
        DataType::Int16 => "short".to_string(),
        DataType::Int32 => "integer".to_string(),
        DataType::Int64 => "long".to_string(),
        DataType::UInt8 => "short".to_string(),
        DataType::UInt16 => "integer".to_string(),
        DataType::UInt32 => "long".to_string(),
        DataType::UInt64 => "decimal(20,0)".to_string(),
        DataType::Float32 => "float".to_string(),
        DataType::Float64 => "double".to_string(),
        DataType::Utf8 | DataType::LargeUtf8 => "string".to_string(),
        // A UUID key is `FixedSizeBinary(16)`: one Delta type, `binary`,
        // covers every byte string — the width is the writer's business.
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => {
            "binary".to_string()
        }
        DataType::Date32 => "date".to_string(),
        DataType::Timestamp(arrow::datatypes::TimeUnit::Microsecond, Some(tz))
            if tz.as_ref() == "UTC" =>
        {
            "timestamp".to_string()
        }
        DataType::Timestamp(arrow::datatypes::TimeUnit::Microsecond, _) => {
            "timestamp_ntz".to_string()
        }
        DataType::Decimal128(p, s) => format!("decimal({p},{s})"),
        DataType::Struct(fields) => {
            let inner = fields
                .iter()
                .map(|f| {
                    let t = delta_type(f)?;
                    Some(format!("\"{}\":\"{}\"", f.name(), t))
                })
                .collect::<Option<Vec<_>>>()?;
            format!("struct<{}>", inner.join(","))
        }
        DataType::List(fields) => {
            let t = delta_type(fields)?;
            format!("array<{t}>")
        }
        _ => return None,
    })
}

/// The fields an existing schema declares, in order, with any NEW ones appended.
///
/// A commit describes a table, not the file that happened to trigger it: dropping a field
/// because this window did not carry it would hand a reader a table that quietly lost a
/// column (and the Delta metadata would disagree with the files already in `data/`).
fn union_columns(existing_schema: &str, file: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(existing_schema)
        && let Some(fields) = parsed["fields"].as_array()
    {
        for field in fields {
            if let (Some(name), Some(ty)) = (field["name"].as_str(), field["type"].as_str()) {
                out.push((name.to_string(), ty.to_string()));
            }
        }
    }
    for (name, ty) in file {
        if !out.iter().any(|(kept, _)| kept == name) {
            out.push((name.clone(), ty.clone()));
        }
    }
    out
}

/// The partition values that place a file in the table's key space, by column.
pub type Partition = Vec<(String, String)>;

/// One file an `add` action names, and the partition values that place it.
///
/// A partitioned table keeps one directory per value (`data/org=1/…`), and Delta carries the
/// same value as a STRING in the action — the directory is a convention a reader may use, the
/// action is what a reader is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addition {
    /// Where the file is on disk.
    pub path: PathBuf,
    /// `(column, value)` for each partition column; empty for an unpartitioned table.
    pub partition: Partition,
}

impl Addition {
    pub fn plain(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            partition: Vec::new(),
        }
    }
}

/// The Delta schema JSON: a struct of the columns, by NAME. Delta identifies columns by
/// name, so the Parquet field ids okm stamps for its own readers are not needed here.
///
/// Key columns carry `{"okm.key": "true"}` in their metadata. That is the one thing a
/// reader cannot derive from the file: which columns are a document's IDENTITY, and
/// therefore what "the same row" means when history is folded into current state.
fn schema_json(columns: &[(String, String)], keys: &[String]) -> String {
    let fields: Vec<String> = columns
        .iter()
        .map(|(name, ty)| {
            let metadata = if keys.iter().any(|k| k == name) {
                "{\"okm.key\":\"true\"}".to_string()
            } else {
                "{}".to_string()
            };
            format!(
                "{{\"name\":{},\"type\":{},\"nullable\":true,\"metadata\":{}}}",
                serde_json::Value::String(name.clone()),
                serde_json::Value::String(ty.clone()),
                metadata
            )
        })
        .collect();
    format!("{{\"type\":\"struct\",\"fields\":[{}]}}", fields.join(","))
}

/// What the log already says — read back so a commit knows what it is adding to.
#[derive(Default)]
struct LogState {
    max_version: Option<u64>,
    table_id: Option<String>,
    schema: Option<String>,
    /// Membership: what a reader would see now.
    files: BTreeSet<String>,
    /// The same files in the order the log added them — a window's order IS the version
    /// that added it, and folding history needs that order, not a name sort.
    order: Vec<String>,
    /// Every file a `remove` action took out of the table, and the version that did it —
    /// what a vacuum needs to tell "history still covers this file" from "nothing does".
    removed: Vec<(String, u64)>,
    /// The columns the table is partitioned by, as `metaData` declares them.
    partition: Vec<String>,
    /// Where each live file sits in the key space, by path.
    partitions: HashMap<String, Partition>,
}

fn read_log(table_dir: &Path) -> Result<LogState, String> {
    let log = table_dir.join("_delta_log");
    let mut state = LogState::default();
    if !log.is_dir() {
        return Ok(state);
    }
    let mut versions: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&log).map_err(|e| format!("{}: {e}", log.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        // Only `NNN.json` counts: `.json.tmp` is a commit in flight and `.checkpoint.`
        // files are a reader's optimisation this writer never makes.
        let is_json = path.extension().is_some_and(|e| e == "json");
        let version = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok());
        if let (true, Some(v)) = (is_json, version) {
            versions.push((v, path));
        }
    }
    versions.sort();
    for (v, path) in versions {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let action: serde_json::Value =
                serde_json::from_str(line).map_err(|e| format!("{}: {e}", path.display()))?;
            if let Some(meta) = action.get("metaData") {
                if let Some(id) = meta.get("id").and_then(|v| v.as_str()) {
                    state.table_id = Some(id.to_string());
                }
                if let Some(schema) = meta.get("schemaString").and_then(|v| v.as_str()) {
                    state.schema = Some(schema.to_string());
                }
                if let Some(columns) = meta.get("partitionColumns").and_then(|v| v.as_array()) {
                    state.partition = columns
                        .iter()
                        .filter_map(|c| c.as_str().map(|c| c.to_string()))
                        .collect();
                }
            }
            if let Some(add) = action.get("add")
                && let Some(p) = add.get("path").and_then(|v| v.as_str())
            {
                let values = add
                    .get("partitionValues")
                    .and_then(|v| v.as_object())
                    .map(|object| {
                        object
                            .iter()
                            .filter_map(|(name, value)| {
                                value
                                    .as_str()
                                    .map(|value| (name.clone(), value.to_string()))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                state.partitions.insert(p.to_string(), values);
                if state.files.insert(p.to_string()) {
                    state.order.push(p.to_string());
                }
            }
            if let Some(remove) = action.get("remove")
                && let Some(p) = remove.get("path").and_then(|v| v.as_str())
            {
                state.files.remove(p);
                state.order.retain(|kept| kept != p);
                state.removed.push((p.to_string(), v));
            }
        }
        state.max_version = Some(v);
    }
    Ok(state)
}

/// A table id that is stable for a directory: Delta only requires uniqueness, and a
/// drain re-run must not invent a new identity for the same table.
fn table_id(table_dir: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    table_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .hash(&mut hasher);
    let h = hasher.finish();
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (h >> 32) as u32,
        (h >> 16) as u16,
        (h >> 4) as u16 & 0x0fff,
        (h as u16) & 0x0fff,
        h & 0x000f_ffff_ffff_ffff
    )
}

/// Commit `files` (each already written into `<table_dir>/data/`) as the next version.
///
/// A file already named in the log is skipped, so draining twice over the same window
/// cannot double-count it. The version file is written under a `.tmp` name and renamed
/// into place: a reader that lists `NNN.json` never sees a half-written commit.
pub fn commit_files(
    table_dir: &Path,
    files: &[Addition],
    keys: &[String],
) -> Result<Commit, String> {
    let log = table_dir.join("_delta_log");
    std::fs::create_dir_all(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    let state = read_log(table_dir)?;

    if files.is_empty() {
        return Err("no data files to commit".to_string());
    }

    // Which files are new, and what each one holds. A path is recorded relative to the
    // table directory, which is what the specification wants and what makes the table
    // relocatable — including the partition directory a file sits in.
    struct Add {
        path: String,
        partition: Partition,
        size: u64,
        stats: String,
    }
    let mut adds: Vec<Add> = Vec::new();
    let mut summary: Option<FileSummary> = None;
    let mut partition: Vec<String> = Vec::new();
    for file in files {
        let relative = file
            .path
            .strip_prefix(table_dir)
            .map_err(|_| {
                format!(
                    "{}: not inside {}",
                    file.path.display(),
                    table_dir.display()
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        let size = std::fs::metadata(&file.path)
            .map_err(|e| format!("{}: {e}", file.path.display()))?
            .len();
        let summary_of_file = file_summary(&file.path)?;
        // A table is partitioned by ONE set of columns for its whole life: a reader that was
        // told `partitionColumns` reuses that answer, so a window may not change it.
        let names: Vec<String> = file
            .partition
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        if partition.is_empty() {
            partition = names;
        } else if partition != names {
            return Err(format!(
                "this table is partitioned by {partition:?}; this window would write {names:?}"
            ));
        }
        if !state.files.contains(&relative) {
            adds.push(Add {
                path: relative,
                partition: file.partition.clone(),
                size,
                stats: stats_json(&summary_of_file),
            });
        }
        summary.get_or_insert(summary_of_file);
    }
    if !state.partition.is_empty() && partition != state.partition {
        return Err(format!(
            "this table is partitioned by {:?}; this window would write {partition:?}",
            state.partition
        ));
    }
    if state.partition.is_empty() && !partition.is_empty() && state.max_version.is_some() {
        return Err(format!(
            "this table was written unpartitioned; a window cannot start partitioning it by {partition:?}"
        ));
    }

    // Every file a window writes carries the declared schema, so any of them describes
    // the table; the first one we looked at is the one the metadata quotes.
    let summary = summary.ok_or_else(|| "no data files to commit".to_string())?;
    // The table's schema is the UNION of what its files carry, never narrowed by the last
    // one: the `variant` column shows up only in windows that wrote open fields, and a
    // window without them must not retract the column from a table that already declared
    // it — a reader would lose the file's other half (ADR-0058 § Amendment).
    let columns = match state.schema.as_deref() {
        Some(existing) => union_columns(existing, &summary.columns),
        None => summary.columns.clone(),
    };
    let schema = schema_json(&columns, keys);
    let has_variant = columns.iter().any(|(_, ty)| ty == "variant");

    let version = state.max_version.map(|v| v + 1).unwrap_or(0);
    let mut actions: Vec<serde_json::Value> = Vec::new();

    // The protocol is written once, and again only if a variant column changes the
    // features — a table that starts as plain parquet must say so before a reader
    // takes its schema for the truth.
    if version == 0 || has_variant {
        let mut protocol = serde_json::json!({
            "minReaderVersion": if has_variant { 3 } else { 1 },
            "minWriterVersion": if has_variant { 7 } else { 2 },
        });
        if has_variant {
            protocol["readerFeatures"] = serde_json::json!(["variantType"]);
            protocol["writerFeatures"] = serde_json::json!(["variantType"]);
        }
        actions.push(serde_json::json!({ "protocol": protocol }));
    }

    // The schema is written once, and again when it changed (a new declared field).
    if state.schema.as_deref() != Some(schema.as_str()) {
        actions.push(serde_json::json!({
            "metaData": {
                "id": state.table_id.clone().unwrap_or_else(|| table_id(table_dir)),
                "format": { "provider": "parquet", "options": {} },
                "schemaString": schema,
                "partitionColumns": partition,
                "configuration": {},
                "createdTime": now_ms(),
            }
        }));
    }

    let modification = now_ms();
    for add in &adds {
        actions.push(serde_json::json!({
            "add": {
                "path": add.path,
                "partitionValues": partition_object(&add.partition),
                "size": add.size,
                "modificationTime": modification,
                "dataChange": true,
                "stats": add.stats,
            }
        }));
    }

    let added = adds.len();
    write_actions(&log, version, &actions, added)
}

/// A partition's values as Delta states them: strings, keyed by column.
fn partition_object(partition: &Partition) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for (name, value) in partition {
        object.insert(name.clone(), serde_json::Value::String(value.clone()));
    }
    serde_json::Value::Object(object)
}

/// One commit: the actions, written under a scratch name and renamed into place.
fn write_actions(
    log: &Path,
    version: u64,
    actions: &[serde_json::Value],
    added: usize,
) -> Result<Commit, String> {
    let path = log.join(format!("{version:020}.json"));
    // A `serde_json::Value` always serialises; the error would mean a bug in this
    // function's own construction, so it is reported rather than assumed away.
    let mut body = String::new();
    for action in actions {
        let line = serde_json::to_string(action).map_err(|e| format!("action: {e}"))?;
        body.push_str(&line);
        body.push('\n');
    }
    let scratch = log.join(format!("{version:020}.json.tmp"));
    std::fs::write(&scratch, body).map_err(|e| format!("{}: {e}", scratch.display()))?;
    std::fs::rename(&scratch, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Commit {
        version,
        path,
        added,
    })
}

/// the table's schema string (which is where the key columns are marked).
pub fn files_in_order(table_dir: &Path) -> Result<Vec<String>, String> {
    Ok(read_log(table_dir)?.order)
}

/// The table's live files with the partition values each one carries, in log order.
///
/// A fold writes its output into the same partition directories as the files it replaces, so
/// it has to know which value each file belongs to — the log says, the path only hints.
pub fn live_adds(table_dir: &Path) -> Result<Vec<(String, Partition)>, String> {
    let state = read_log(table_dir)?;
    Ok(state
        .order
        .into_iter()
        .map(|path| {
            let values = state.partitions.get(&path).cloned().unwrap_or_default();
            (path, values)
        })
        .collect())
}

/// Files that left the table, with the version that removed them, and the newest version.
///
/// A file that was removed and later re-added with the same path is not listed: it is in the
/// table again, and deleting it would delete a live file. That cannot happen with this
/// writer's names (a fold's name carries its version), but the check costs one set lookup.
pub fn removed_files(table_dir: &Path) -> Result<(Vec<(String, u64)>, u64), String> {
    let state = read_log(table_dir)?;
    let latest = state.max_version.unwrap_or(0);
    let gone = state
        .removed
        .into_iter()
        .filter(|(path, _)| !state.files.contains(path))
        .collect();
    Ok((gone, latest))
}

/// Every file the table has ever added — the set a vacuum deletes from disk. A path here that
/// is not in `files_in_order` is a file the table no longer reads.
pub fn added_files(table_dir: &Path) -> Result<Vec<String>, String> {
    let state = read_log(table_dir)?;
    let mut all: Vec<String> = state.order;
    all.extend(state.removed.into_iter().map(|(path, _)| path));
    all.sort();
    all.dedup();
    Ok(all)
}

/// The version a commit would take next.
pub fn next_version(table_dir: &Path) -> Result<u64, String> {
    Ok(read_log(table_dir)?.max_version.map(|v| v + 1).unwrap_or(0))
}

/// The table's current schema string, if it has one yet.
pub fn schema_string(table_dir: &Path) -> Result<Option<String>, String> {
    Ok(read_log(table_dir)?.schema)
}

/// One commit that REPLACES files: every `removes` entry leaves the table, `add` joins it.
///
/// This is the shape a fold needs — dropping history for a file that holds current state of
/// the same table. Protocol and schema are not rewritten: the replacing files carry the same
/// columns as the ones they replace, and a commit that restated the schema would only give a
/// reader a second chance to disagree with it.
pub fn commit_compaction(
    table_dir: &Path,
    removes: &[String],
    adds: &[Addition],
    keys: &[String],
) -> Result<Commit, String> {
    if removes.is_empty() {
        return Err("no files to remove: a fold that replaces nothing is a drain".to_string());
    }
    // A fold may resolve every key away (the table is now empty); then it removes files
    // and adds none, which is a state a reader has to be able to see.
    let log = table_dir.join("_delta_log");
    std::fs::create_dir_all(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    let state = read_log(table_dir)?;
    for path in removes {
        if !state.files.contains(path) {
            return Err(format!("{path} is not in this table's log"));
        }
    }

    let version = state.max_version.map(|v| v + 1).unwrap_or(0);
    let mut actions: Vec<serde_json::Value> = Vec::new();
    if version == 0 || state.schema.is_none() {
        return Err("the folded file does not carry the table's schema".to_string());
    }

    // Every add first, so the schema is checked against what the table already declares: a
    // fold that changed a column would be a different table, not a fold of this one.
    struct Add {
        path: String,
        partition: Vec<(String, String)>,
        size: u64,
        stats: String,
    }
    let mut kept: Vec<Add> = Vec::new();
    for addition in adds {
        let relative = addition
            .path
            .strip_prefix(table_dir)
            .map_err(|_| {
                format!(
                    "{}: not inside {}",
                    addition.path.display(),
                    table_dir.display()
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        let summary = file_summary(&addition.path)?;
        if schema_json(
            &union_columns(&state.schema.clone().unwrap_or_default(), &summary.columns),
            keys,
        ) != state.schema.clone().unwrap_or_default()
        {
            return Err(format!(
                "{}: the folded file does not carry the table's schema",
                addition.path.display()
            ));
        }
        kept.push(Add {
            path: relative,
            partition: addition.partition.clone(),
            size: std::fs::metadata(&addition.path)
                .map_err(|e| format!("{}: {e}", addition.path.display()))?
                .len(),
            stats: stats_json(&summary),
        });
    }

    let stamp = now_ms();
    for path in removes {
        actions.push(serde_json::json!({
            "remove": {
                "path": path,
                "deletionTimestamp": stamp,
                "dataChange": true,
            }
        }));
    }
    for add in &kept {
        actions.push(serde_json::json!({
            "add": {
                "path": add.path,
                "partitionValues": partition_object(&add.partition),
                "size": add.size,
                "modificationTime": stamp,
                "dataChange": true,
                "stats": add.stats,
            }
        }));
    }

    let added = kept.len();
    write_actions(&log, version, &actions, added)
}

/// without opening it; a file whose statistics were not computed gets the row count only.
fn stats_json(file: &FileSummary) -> String {
    let mut stats = serde_json::Map::new();
    stats.insert("numRecords".into(), serde_json::Value::from(file.rows));
    if !file.min.is_empty() {
        stats.insert(
            "minValues".into(),
            serde_json::Value::Object(file.min.clone()),
        );
        stats.insert(
            "maxValues".into(),
            serde_json::Value::Object(file.max.clone()),
        );
    }
    if !file.nulls.is_empty() {
        stats.insert(
            "nullCount".into(),
            serde_json::Value::Object(file.nulls.clone()),
        );
    }
    serde_json::Value::Object(stats).to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
