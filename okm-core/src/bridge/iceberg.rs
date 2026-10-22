//! Iceberg table metadata for an exported directory: the files stay THIS
//! crate's Parquet (the bridge wrote them), and the metadata is what turns a
//! pile of `.parquet` files into something a warehouse reader opens as a
//! table (`iceberg_scan`, pyiceberg, a catalog register).
//!
//! No Iceberg or Avro crate is involved. The metadata is JSON and the two
//! manifests are Avro object container files — a header, a schema, and one
//! block — so the writer below is an encoder for exactly the field set the
//! spec fixes, rendered twice from ONE description: the Avro schema JSON in
//! the file header and the value bytes. The per-field `field-id` attribute is
//! not optional: a manifest without it is unreadable (pyiceberg refuses with
//! "missing field-id", DuckDB asserts in its column mapper), and the ids are
//! the spec's own numbering, mirrored here.
//!
//! The same numbering is stamped onto the Parquet files' columns as
//! `PARQUET:field_id` (see [`stamp_field_ids`]), because Iceberg identifies a
//! column by ID and only falls back to names when a table declares a name
//! mapping. Column order is the declaration order, so the two sides agree by
//! construction: position `i` is field id `i + 1` everywhere.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};
use std::sync::Arc;

/// The Parquet key-value/field metadata key that carries a column's Iceberg
/// field id. arrow-rs writes it into the Parquet schema as `field_id`.
pub const FIELD_ID_KEY: &str = "PARQUET:field_id";

/// Stamp `1..=n` onto a schema's fields, in column order — the numbering an
/// Iceberg schema generated from this table will carry.
pub fn stamp_field_ids(schema: &ArrowSchema) -> ArrowSchema {
    let fields: Vec<ArrowField> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let mut metadata = f.metadata().clone();
            metadata.insert(FIELD_ID_KEY.to_string(), (i as i64 + 1).to_string());
            f.as_ref().clone().with_metadata(metadata)
        })
        .collect();
    ArrowSchema::new_with_metadata(fields, schema.metadata().clone())
}

/// The same stamping for a batch about to be written: the returned batch's
/// schema is the stamped one, so the writer's schema and the data agree.
pub fn stamp_batch(batch: &RecordBatch) -> RecordBatch {
    let schema = Arc::new(stamp_field_ids(batch.schema().as_ref()));
    RecordBatch::try_new(schema, batch.columns().to_vec())
        .expect("stamping field ids keeps every column in place")
}

// ---------------------------------------------------------------------------
// Avro: just enough for the two Iceberg manifest schemas
// ---------------------------------------------------------------------------

/// An Avro type, restricted to what the manifest schemas use. `field-id` /
/// `element-id` ride along so the schema JSON and the encoder come from one
/// description — the spec's attributes are part of the type, not of a string
/// template somewhere else.
#[derive(Clone)]
enum Avro {
    Int,
    Long,
    Bool,
    Str,
    Bytes,
    /// `["null", inner]` — every optional field in these schemas is this shape.
    Opt(Box<Avro>),
    Record {
        name: &'static str,
        fields: Vec<AvroField>,
    },
    Array {
        element_id: i64,
        items: Box<Avro>,
    },
    /// An array-of-key/value-record that a reader must bind as a MAP. Iceberg
    /// spells maps this way in Avro (non-string keys have no other form), and
    /// the `logicalType` is what tells a reader it is a map: without it DuckDB
    /// binds the column as `STRUCT(key, value)[]` and then fails to convert it
    /// into the `MAP` its Iceberg layer expects.
    MapArray {
        element_id: i64,
        items: Box<Avro>,
    },
}

#[derive(Clone)]
struct AvroField {
    name: &'static str,
    field_id: i64,
    ty: Avro,
}

fn field(name: &'static str, field_id: i64, ty: Avro) -> AvroField {
    AvroField { name, field_id, ty }
}

/// The array-of-key/value record shape Iceberg uses to spell a map in Avro.
fn avro_map(
    name: &'static str,
    field_id: i64,
    key_id: i64,
    value_id: i64,
    value: Avro,
    record: &'static str,
) -> AvroField {
    let items = Avro::Record {
        name: record,
        fields: vec![
            field("key", key_id, Avro::Int),
            field("value", value_id, value),
        ],
    };
    field(
        name,
        field_id,
        Avro::Opt(Box::new(Avro::MapArray {
            element_id: field_id,
            items: Box::new(items),
        })),
    )
}

impl Avro {
    fn json(&self) -> String {
        match self {
            Avro::Int => "\"int\"".into(),
            Avro::Long => "\"long\"".into(),
            Avro::Bool => "\"boolean\"".into(),
            Avro::Str => "\"string\"".into(),
            Avro::Bytes => "\"bytes\"".into(),
            Avro::Opt(inner) => format!("[\"null\",{}]", inner.json()),
            Avro::Record { name, fields } => {
                let mut s = String::from("{\"type\":\"record\",\"name\":\"");
                s.push_str(name);
                s.push_str("\",\"fields\":[");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    let _ = write!(
                        s,
                        "{{\"name\":\"{}\",\"field-id\":{},\"type\":{}{}}}",
                        f.name,
                        f.field_id,
                        f.ty.json(),
                        if matches!(f.ty, Avro::Opt(_)) {
                            ",\"default\":null"
                        } else {
                            ""
                        }
                    );
                }
                s.push_str("]}");
                s
            }
            Avro::Array { element_id, items } => format!(
                "{{\"type\":\"array\",\"element-id\":{},\"items\":{}}}",
                element_id,
                items.json()
            ),
            Avro::MapArray { element_id, items } => format!(
                "{{\"type\":\"array\",\"element-id\":{},\"items\":{},\"logicalType\":\"map\"}}",
                element_id,
                items.json()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Avro binary encoding (zig-zag longs, length-prefixed bytes/strings)
// ---------------------------------------------------------------------------

fn zigzag(n: i64) -> Vec<u8> {
    let mut n = ((n << 1) ^ (n >> 63)) as u64;
    let mut out = Vec::with_capacity(10);
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    out
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend(zigzag(s.len() as i64));
    out.extend_from_slice(s.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend(zigzag(b.len() as i64));
    out.extend_from_slice(b);
}

/// One manifest row. Plain data, so the two writers below never walk a
/// generic value tree.
struct ManifestEntry {
    status: i32,
    snapshot_id: i64,
    sequence_number: i64,
    file_path: String,
    file_size: i64,
    record_count: i64,
}

fn manifest_schema() -> Avro {
    Avro::Record {
        name: "manifest_entry",
        fields: vec![
            field("status", 0, Avro::Int),
            field("snapshot_id", 1, Avro::Opt(Box::new(Avro::Long))),
            field("sequence_number", 3, Avro::Opt(Box::new(Avro::Long))),
            field("file_sequence_number", 4, Avro::Opt(Box::new(Avro::Long))),
            field(
                "data_file",
                2,
                Avro::Record {
                    name: "r2",
                    fields: vec![
                        field("content", 134, Avro::Int),
                        field("file_path", 100, Avro::Str),
                        field("file_format", 101, Avro::Str),
                        field(
                            "partition",
                            102,
                            Avro::Record {
                                name: "r102",
                                fields: vec![],
                            },
                        ),
                        field("record_count", 103, Avro::Long),
                        field("file_size_in_bytes", 104, Avro::Long),
                        avro_map("column_sizes", 108, 117, 118, Avro::Long, "k117_v118"),
                        avro_map("value_counts", 109, 119, 120, Avro::Long, "k119_v120"),
                        avro_map("null_value_counts", 110, 121, 122, Avro::Long, "k121_v122"),
                        avro_map("nan_value_counts", 137, 138, 139, Avro::Long, "k138_v139"),
                        avro_map("lower_bounds", 125, 126, 127, Avro::Bytes, "k126_v127"),
                        avro_map("upper_bounds", 128, 129, 130, Avro::Bytes, "k129_v130"),
                        field("key_metadata", 131, Avro::Opt(Box::new(Avro::Bytes))),
                        field(
                            "split_offsets",
                            132,
                            Avro::Opt(Box::new(Avro::Array {
                                element_id: 133,
                                items: Box::new(Avro::Long),
                            })),
                        ),
                        field(
                            "equality_ids",
                            135,
                            Avro::Opt(Box::new(Avro::Array {
                                element_id: 136,
                                items: Box::new(Avro::Long),
                            })),
                        ),
                        field("sort_order_id", 140, Avro::Opt(Box::new(Avro::Int))),
                    ],
                },
            ),
        ],
    }
}

fn manifest_list_schema() -> Avro {
    Avro::Record {
        name: "manifest_file",
        fields: vec![
            field("manifest_path", 500, Avro::Str),
            field("manifest_length", 501, Avro::Long),
            field("partition_spec_id", 502, Avro::Int),
            field("content", 517, Avro::Int),
            field("sequence_number", 515, Avro::Long),
            field("min_sequence_number", 516, Avro::Long),
            field("added_snapshot_id", 503, Avro::Long),
            field("added_files_count", 504, Avro::Int),
            field("existing_files_count", 505, Avro::Int),
            field("deleted_files_count", 506, Avro::Int),
            field("added_rows_count", 512, Avro::Long),
            field("existing_rows_count", 513, Avro::Long),
            field("deleted_rows_count", 514, Avro::Long),
            field(
                "partitions",
                507,
                Avro::Opt(Box::new(Avro::Array {
                    element_id: 508,
                    items: Box::new(Avro::Record {
                        name: "r508",
                        fields: vec![
                            field("contains_null", 509, Avro::Bool),
                            field("contains_nan", 518, Avro::Opt(Box::new(Avro::Bool))),
                            field("lower_bound", 510, Avro::Opt(Box::new(Avro::Bytes))),
                            field("upper_bound", 511, Avro::Opt(Box::new(Avro::Bytes))),
                        ],
                    }),
                })),
            ),
            field("key_metadata", 519, Avro::Opt(Box::new(Avro::Bytes))),
        ],
    }
}

impl ManifestEntry {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend(zigzag(self.status as i64));
        out.extend(zigzag(1)); // snapshot_id: union branch 1 (long)
        out.extend(zigzag(self.snapshot_id));
        out.extend(zigzag(1)); // sequence_number
        out.extend(zigzag(self.sequence_number));
        out.extend(zigzag(1)); // file_sequence_number
        out.extend(zigzag(self.sequence_number));
        // data_file
        out.extend(zigzag(0)); // content = DATA
        put_str(out, &self.file_path);
        put_str(out, "PARQUET");
        // partition: an empty record for an unpartitioned table
        out.extend(zigzag(self.record_count));
        out.extend(zigzag(self.file_size));
        for _ in 0..6 {
            out.extend(zigzag(0)); // column_sizes, value_counts, null/nan counts,
            // lower/upper bounds: absent
        }
        out.extend(zigzag(0)); // key_metadata
        out.extend(zigzag(0)); // split_offsets
        out.extend(zigzag(0)); // equality_ids
        out.extend(zigzag(0)); // sort_order_id
    }
}

/// Wrap one Avro block in the object container format: magic, the metadata
/// map (`avro.schema` + `avro.codec`), a sync marker, then the block.
fn write_ocf(path: &Path, schema: &Avro, body: &[u8], rows: usize) -> std::io::Result<()> {
    const SYNC: &[u8; 16] = b"okm-iceberg-sync";
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(b"Obj\x01");
    out.extend(zigzag(2)); // two metadata entries
    put_str(&mut out, "avro.schema");
    put_bytes(&mut out, schema.json().as_bytes());
    put_str(&mut out, "avro.codec");
    put_bytes(&mut out, b"null");
    out.extend(zigzag(0)); // end of the metadata map
    out.extend_from_slice(SYNC);
    out.extend(zigzag(rows as i64));
    out.extend(zigzag(body.len() as i64));
    out.extend_from_slice(body);
    out.extend_from_slice(SYNC);
    std::fs::write(path, out)
}

// ---------------------------------------------------------------------------
// Iceberg schema types
// ---------------------------------------------------------------------------

/// The Iceberg type a column declares. Mapped from the ARROW type the bridge
/// writes, so the metadata can never claim a type the file does not hold:
/// a reader validates the physical Parquet type against this, and a mismatch
/// is a refused table rather than a wrong one.
fn iceberg_type(ty: &DataType) -> Option<String> {
    Some(match ty {
        DataType::Boolean => "\"boolean\"".into(),
        DataType::Int8 | DataType::Int16 | DataType::Int32 => "\"int\"".into(),
        DataType::UInt8 | DataType::UInt16 => "\"int\"".into(),
        DataType::UInt32 | DataType::Int64 | DataType::UInt64 => "\"long\"".into(),
        DataType::Float32 => "\"float\"".into(),
        DataType::Float64 => "\"double\"".into(),
        DataType::Utf8 | DataType::LargeUtf8 => "\"string\"".into(),
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => {
            "\"binary\"".into()
        }
        // A column type Iceberg cannot name is a REFUSAL, not a guess: the
        // metadata claims what the file holds, or it is not written.
        _ => return None,
    })
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn uuid_like() -> String {
    // No uuid dependency: 16 bytes from the clock and the process id, spelled
    // the way Iceberg spells ids (the value only has to be unique within one
    // warehouse, and a re-export overwrites the metadata file anyway).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let hi = (nanos as u128) ^ (pid << 64);
    let lo = ((nanos >> 64) as u128) ^ pid;
    let bytes = (hi.to_le_bytes(), lo.to_le_bytes());
    let hex: String = bytes
        .0
        .iter()
        .chain(bytes.1.iter())
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// A path as the METADATA records it: RELATIVE to the table directory whenever
/// the file lives inside it, `/`-separated.
///
/// This is what makes a bundle relocatable: the export is written in a temp
/// directory and read after being moved (or on another machine), so an absolute
/// URI recorded here would point at a directory that no longer exists. Iceberg
/// resolves a relative path against the table's location, which for a directory
/// warehouse is the directory the reader was pointed at — so the file travels.
/// A file OUTSIDE the table directory has no relative form and keeps its
/// absolute URI (the caller placed it somewhere this writer cannot describe).
fn recorded_path(table_dir: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(table_dir)
        .or_else(|_| path.strip_prefix(table_dir.canonicalize().unwrap_or_default()));
    match relative {
        Ok(inside) => inside
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => {
            let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            format!("file://{}", abs.display())
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write one table's Iceberg metadata for data files that ALREADY exist:
/// the manifest, the manifest list, `v1.metadata.json` and
/// `version-hint.text` (DuckDB reads a table DIRECTORY through that hint;
/// pyiceberg and a catalog take the metadata path directly).
///
/// The columns and row counts are read FROM THE FILES, never restated by the
/// caller: a metadata writer that takes its own copy of the column list can
/// disagree with what it describes, and the file is the thing a reader opens.
/// Returns the metadata file's path.
pub fn write_table_metadata(table_dir: &Path, data_files: &[PathBuf]) -> Result<PathBuf, String> {
    let Some(first) = data_files.first() else {
        return Err("no data files: a table needs at least one".into());
    };
    let (columns, _) = file_summary(first)?;
    let mut files: Vec<(PathBuf, u64)> = Vec::with_capacity(data_files.len());
    for path in data_files {
        let (_, rows) = file_summary(path)?;
        files.push((path.clone(), rows));
    }
    write_with_columns(table_dir, &columns, &files).map_err(|e| e.to_string())
}

/// A Parquet file's column list `(name, arrow type)` in file order, and its row
/// count — read from the file itself (the same `parquet` crate the writer uses).
pub fn file_summary(path: &Path) -> Result<(Vec<(String, DataType)>, u64), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let schema = reader.schema();
    let columns: Vec<(String, DataType)> = schema
        .fields()
        .iter()
        .map(|f| (f.name().clone(), f.data_type().clone()))
        .collect();
    let rows = reader.metadata().file_metadata().num_rows();
    Ok((columns, rows.max(0) as u64))
}

fn write_with_columns(
    table_dir: &Path,
    columns: &[(String, DataType)],
    data_files: &[(PathBuf, u64)],
) -> std::io::Result<PathBuf> {
    let meta_dir = table_dir.join("metadata");
    std::fs::create_dir_all(&meta_dir)?;

    let snapshot_id = 6_000_000_000_000_000_000u64 + (now_ms() % 1_000_000) as u64;
    let sequence = 1i64;
    let now = now_ms();

    // Manifest: one entry per data file.
    let uuid = uuid_like();
    let manifest_path = meta_dir.join(format!("{uuid}-m0.avro"));
    let mut body = Vec::new();
    for (path, rows) in data_files {
        let entry = ManifestEntry {
            status: 1, // ADDED
            snapshot_id: snapshot_id as i64,
            sequence_number: sequence,
            file_path: recorded_path(table_dir, path),
            file_size: std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0),
            record_count: *rows as i64,
        };
        entry.encode(&mut body);
    }
    write_ocf(&manifest_path, &manifest_schema(), &body, data_files.len())?;
    let manifest_len = std::fs::metadata(&manifest_path)?.len() as i64;

    // Manifest list: one row naming that manifest.
    let list_path = meta_dir.join(format!("snap-{snapshot_id}-0-{uuid}.avro"));
    let mut list = Vec::new();
    put_str(&mut list, &recorded_path(table_dir, &manifest_path));
    list.extend(zigzag(manifest_len));
    list.extend(zigzag(0)); // partition_spec_id
    list.extend(zigzag(0)); // content = DATA
    list.extend(zigzag(sequence));
    list.extend(zigzag(sequence)); // min_sequence_number
    list.extend(zigzag(snapshot_id as i64));
    list.extend(zigzag(data_files.len() as i64)); // added_files_count
    list.extend(zigzag(0)); // existing_files_count
    list.extend(zigzag(0)); // deleted_files_count
    list.extend(zigzag(data_files.iter().map(|(_, n)| *n as i64).sum())); // added_rows_count
    list.extend(zigzag(0)); // existing_rows_count
    list.extend(zigzag(0)); // deleted_rows_count
    list.extend(zigzag(0)); // partitions: absent
    list.extend(zigzag(0)); // key_metadata
    write_ocf(&list_path, &manifest_list_schema(), &list, 1)?;

    let total_rows: u64 = data_files.iter().map(|(_, n)| *n).sum();
    let schema_fields: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, (name, ty))| {
            let iceberg = iceberg_type(ty).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("column `{name}` has no Iceberg type for {ty}"),
                )
            })?;
            Ok(format!(
                "{{\"id\":{},\"name\":\"{}\",\"type\":{},\"required\":false}}",
                i + 1,
                json_escape(name),
                iceberg
            ))
        })
        .collect::<std::io::Result<Vec<String>>>()?;

    let metadata = format!(
        concat!(
            "{{\n",
            " \"format-version\": 2,\n",
            " \"table-uuid\": \"{uuid}\",\n",
            " \"location\": \"{location}\",\n",
            " \"last-sequence-number\": {sequence},\n",
            " \"last-updated-ms\": {now},\n",
            " \"last-column-id\": {last_column},\n",
            " \"schemas\": [{{ \"type\": \"struct\", \"schema-id\": 0, \"identifier-field-ids\": [], \"fields\": [{fields}] }}],\n",
            " \"current-schema-id\": 0,\n",
            " \"partition-specs\": [{{ \"spec-id\": 0, \"fields\": [] }}],\n",
            " \"default-spec-id\": 0,\n",
            " \"last-partition-id\": 999,\n",
            " \"properties\": {{}},\n",
            " \"current-snapshot-id\": {snapshot},\n",
            " \"snapshots\": [{{ \"snapshot-id\": {snapshot}, \"sequence-number\": {sequence}, \"timestamp-ms\": {now}, \"manifest-list\": \"{manifest_list}\", \"summary\": {{ \"operation\": \"append\", \"added-data-files\": \"{files}\", \"added-records\": \"{rows}\", \"total-data-files\": \"{files}\", \"total-records\": \"{rows}\" }}, \"schema-id\": 0 }}],\n",
            " \"snapshot-log\": [{{ \"snapshot-id\": {snapshot}, \"timestamp-ms\": {now} }}],\n",
            " \"metadata-log\": [],\n",
            " \"sort-orders\": [{{ \"order-id\": 0, \"fields\": [] }}],\n",
            " \"default-sort-order-id\": 0,\n",
            " \"refs\": {{ \"main\": {{ \"snapshot-id\": {snapshot}, \"type\": \"branch\" }} }},\n",
            " \"statistics\": [],\n",
            " \"partition-statistics\": []\n",
            "}}\n"
        ),
        uuid = uuid,
        location = ".",
        sequence = sequence,
        now = now,
        last_column = columns.len(),
        fields = schema_fields.join(","),
        snapshot = snapshot_id,
        manifest_list = recorded_path(table_dir, &list_path),
        files = data_files.len(),
        rows = total_rows,
    );

    let metadata_path = meta_dir.join("v1.metadata.json");
    std::fs::write(&metadata_path, metadata)?;
    // DuckDB reads a table DIRECTORY through this hint and refuses to guess
    // otherwise; every other reader accepts it as an extra file.
    std::fs::write(meta_dir.join("version-hint.text"), "1")?;
    Ok(metadata_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_ids_are_positions() {
        let schema = ArrowSchema::new(vec![
            ArrowField::new("a", DataType::UInt64, false),
            ArrowField::new("b", DataType::Utf8, true),
        ]);
        let stamped = stamp_field_ids(&schema);
        assert_eq!(
            stamped
                .field(0)
                .metadata()
                .get(FIELD_ID_KEY)
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            stamped
                .field(1)
                .metadata()
                .get(FIELD_ID_KEY)
                .map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn the_metadata_records_paths_a_moved_table_can_still_resolve() {
        // The bundle is written in a temp directory and opened after being
        // moved, so a recorded absolute URI would point at nothing. Everything
        // inside the table directory is relative to it, and the table's own
        // location is the reader's directory.
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t1");
        std::fs::create_dir_all(table.join("data")).unwrap();
        let data = table.join("data/t1.parquet");
        std::fs::write(&data, b"not really parquet").unwrap();
        assert_eq!(recorded_path(&table, &data), "data/t1.parquet");
        let inner = table.join("metadata/x-m0.avro");
        assert_eq!(recorded_path(&table, &inner), "metadata/x-m0.avro");
        // A file outside the table keeps an absolute URI: it has no relative
        // form this writer could describe.
        let outside = dir.path().join("elsewhere.parquet");
        std::fs::write(&outside, b"x").unwrap();
        assert!(recorded_path(&table, &outside).starts_with("file://"));
    }

    #[test]
    fn the_manifest_schema_carries_the_spec_ids() {
        let json = manifest_schema().json();
        // The three ids a reader refuses to do without.
        assert!(json.contains("\"name\":\"status\",\"field-id\":0"));
        assert!(json.contains("\"name\":\"data_file\",\"field-id\":2"));
        assert!(json.contains("\"name\":\"file_path\",\"field-id\":100"));
        let list = manifest_list_schema().json();
        assert!(list.contains("\"name\":\"manifest_path\",\"field-id\":500"));
        assert!(list.contains("\"name\":\"partitions\",\"field-id\":507"));
    }

    #[test]
    fn metadata_lands_where_a_reader_looks() {
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t1");
        let data = table.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let file = data.join("0.parquet");
        std::fs::write(&file, b"not a real parquet, only a size").unwrap();
        let columns: Vec<(String, DataType)> = vec![
            ("id".to_string(), DataType::UInt64),
            ("name".to_string(), DataType::Utf8),
        ];
        let meta = write_with_columns(&table, &columns, &[(file, 3)]).unwrap();
        let text = std::fs::read_to_string(&meta).unwrap();
        assert!(text.contains("\"format-version\": 2"));
        assert!(text.contains("\"name\":\"id\",\"type\":\"long\""));
        assert!(text.contains("\"added-records\": \"3\""));
        assert!(table.join("metadata/version-hint.text").exists());
    }
}
