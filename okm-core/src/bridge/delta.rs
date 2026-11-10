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
//! (measured — see [`crate::bridge::arrow_bridge::variant_parts`]).

use std::collections::BTreeSet;
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
    let rows: u64 = metadata.row_groups().iter().map(|g| g.num_rows() as u64).sum();

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
    for (idx, field) in arrow.fields().iter().enumerate() {
        if skip_unsigned(field.data_type()) {
            continue;
        }
        let mut min: Option<serde_json::Value> = None;
        let mut max: Option<serde_json::Value> = None;
        let mut nulls: u64 = 0;
        let mut counted = true;
        for group in metadata.row_groups() {
            let Some(column) = group.columns().get(idx) else {
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
        (Statistics::Int64(s), DataType::Int64) => {
            Some((num(*s.min_opt()?), num(*s.max_opt()?)))
        }
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
        DataType::Binary | DataType::LargeBinary => "binary".to_string(),
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

/// The Delta schema JSON: a struct of the columns, by NAME. Delta identifies columns by
/// name, so the Parquet field ids okm stamps for its own readers are not needed here.
fn schema_json(columns: &[(String, String)]) -> String {
    let fields: Vec<String> = columns
        .iter()
        .map(|(name, ty)| {
            format!(
                "{{\"name\":{},\"type\":{},\"nullable\":true,\"metadata\":{{}}}}",
                serde_json::Value::String(name.clone()),
                serde_json::Value::String(ty.clone())
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
    files: BTreeSet<String>,
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
            }
            if let Some(add) = action.get("add")
                && let Some(p) = add.get("path").and_then(|v| v.as_str())
            {
                state.files.insert(p.to_string());
            }
            if let Some(remove) = action.get("remove")
                && let Some(p) = remove.get("path").and_then(|v| v.as_str())
            {
                state.files.remove(p);
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
pub fn commit_files(table_dir: &Path, files: &[PathBuf]) -> Result<Commit, String> {
    let log = table_dir.join("_delta_log");
    std::fs::create_dir_all(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    let state = read_log(table_dir)?;

    if files.is_empty() {
        return Err("no data files to commit".to_string());
    }

    // Which files are new, and what each one holds. A path is recorded relative to the
    // table directory, which is what the specification wants and what makes the table
    // relocatable.
    struct Add {
        path: String,
        size: u64,
        stats: String,
    }
    let mut adds: Vec<Add> = Vec::new();
    let mut summary: Option<FileSummary> = None;
    for file in files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{}: not a file name this table can record", file.display()))?;
        let rel = format!("data/{name}");
        let size = std::fs::metadata(file)
            .map_err(|e| format!("{}: {e}", file.display()))?
            .len();
        let file = file_summary(file)?;
        if !state.files.contains(&rel) {
            adds.push(Add {
                path: rel,
                size,
                stats: stats_json(&file),
            });
        }
        summary.get_or_insert(file);
    }

    // Every file a window writes carries the declared schema, so any of them describes
    // the table; the first one we looked at is the one the metadata quotes.
    let summary = summary.ok_or_else(|| "no data files to commit".to_string())?;
    let columns = summary.columns.clone();
    let schema = schema_json(&columns);
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
                "partitionColumns": [],
                // Nothing here is time-partitioned: the export is one directory per
                // table and the reader's filter goes to Parquet statistics.
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
                "partitionValues": {},
                "size": add.size,
                "modificationTime": modification,
                "dataChange": true,
                "stats": add.stats,
            }
        }));
    }

    let path = log.join(format!("{version:020}.json"));
    // A `serde_json::Value` always serialises; the error would mean a bug in this
    // function's own construction, so it is reported rather than assumed away.
    let mut body = String::new();
    for action in &actions {
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
        added: adds.len(),
    })
}

/// The `stats` string a commit carries for one file: `numRecords` plus, where they were
/// measured, `minValues` / `maxValues` / `nullCount`. A reader uses these to skip a file
/// without opening it; a file whose statistics were not computed gets the row count only.
fn stats_json(file: &FileSummary) -> String {
    let mut stats = serde_json::Map::new();
    stats.insert("numRecords".into(), serde_json::Value::from(file.rows));
    if !file.min.is_empty() {
        stats.insert("minValues".into(), serde_json::Value::Object(file.min.clone()));
        stats.insert("maxValues".into(), serde_json::Value::Object(file.max.clone()));
    }
    if !file.nulls.is_empty() {
        stats.insert("nullCount".into(), serde_json::Value::Object(file.nulls.clone()));
    }
    serde_json::Value::Object(stats).to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
