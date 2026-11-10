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
pub fn file_summary(path: &Path) -> Result<(Vec<(String, String)>, u64), String> {
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
    // The row count is the sum over row groups: cheap, and it is all a reader's stats
    // need (`numRecords`).
    let rows = reader
        .metadata()
        .row_groups()
        .iter()
        .map(|g| g.num_rows() as u64)
        .sum();
    Ok((columns, rows))
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
    let mut adds: Vec<(String, u64, u64)> = Vec::new();
    let mut summary: Option<(Vec<(String, String)>, u64)> = None;
    for file in files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{}: not a file name this table can record", file.display()))?;
        let rel = format!("data/{name}");
        let (columns, rows) = file_summary(file)?;
        if state.files.contains(&rel) {
            summary.get_or_insert((columns, rows));
            continue;
        }
        let size = std::fs::metadata(file)
            .map_err(|e| format!("{}: {e}", file.display()))?
            .len();
        adds.push((rel, size, rows));
        summary.get_or_insert((columns, rows));
    }

    // Every file a window writes carries the declared schema, so any of them describes
    // the table; the first one we looked at is the one the metadata quotes.
    let (columns, _) = summary.ok_or_else(|| "no data files to commit".to_string())?;
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
    for (rel, size, rows) in &adds {
        // `numRecords` is per file, so a reader can count without opening it.
        let stats = serde_json::json!({ "numRecords": rows }).to_string();
        actions.push(serde_json::json!({
            "add": {
                "path": rel,
                "partitionValues": {},
                "size": size,
                "modificationTime": modification,
                "dataChange": true,
                "stats": stats,
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
