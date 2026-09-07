//! Tooling interfaces (PLAN Phase 4): layout audit tables, Parquet snapshot
//! export/import.
//!
//! These are library functions, not a standalone binary: `Table<S, K, R>`
//! binds user types at compile time, so the type context must live in the
//! caller's application. A "CLI experience" is a thin bin wrapping these
//! calls.
//!
//! - [`describe`]-style methods render the declaration-order byte layout
//!   (offsets, widths, TLV frame strides) from the same `FieldDesc` tables
//!   the derive macros emit — the declaration is still the single source;
//!   nothing re-parses source code.
//! - `export_parquet` / `import_parquet` (feature `parquet`) sit on the
//!   Arrow bridge: RecordBatch → Parquet file, and back. Import writes
//!   through [`Table::put`] — the normal one-batch write contract (primary
//!   key + index entries) is never bypassed; this is a backup/restore path,
//!   not a second write channel.

use crate::engine::KvEngine;
use crate::field::{FieldDesc, FieldType};
use crate::index::Row;
use crate::key::KeyEncode;
use crate::table::Table;

/// One line of the layout audit table.
pub struct LayoutRow {
    pub name: &'static str,
    /// Byte offset of the value within its region (key encoding for key
    /// fields, TLV payload for row fields).
    pub offset: usize,
    pub width: usize,
    pub ty: FieldType,
}

impl std::fmt::Display for LayoutRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "  {:<16} {:>7} {:>6}  {:?}",
            self.name, self.offset, self.width, self.ty
        )
    }
}

/// Declaration-order offset table for a fixed-width region (the key
/// encoding, or the value half of TLV frames — widths are declared, so the
/// stride math is the same for both).
fn layout_rows(fields: &[FieldDesc]) -> Vec<LayoutRow> {
    let mut rows = Vec::with_capacity(fields.len());
    let mut off = 0usize;
    for f in fields {
        rows.push(LayoutRow {
            name: f.name,
            offset: off,
            width: f.width,
            ty: f.ty,
        });
        off += f.width;
    }
    rows
}

/// Human-readable layout audit for a table's identity + payload fields:
/// key encoding (fixed offsets from 0) and TLV payload (per-field frame
/// stride `tag 1B + len 4B + value`). No compiler session needed — this is
/// the declaration rendered as bytes.
pub fn describe<K: KeyEncode, R: Row<Key = K>>() -> String {
    let key_fields = <K as KeyEncode>::FIELDS;
    let row_fields = <R as Row>::FIELDS;
    let mut out = String::new();
    out.push_str(&format!(
        "{} {{ key: {} }}  KEY_LEN={}\n",
        std::any::type_name::<K>(),
        std::any::type_name::<R>(),
        K::KEY_LEN,
    ));

    out.push_str("  -- key encoding (BE, declaration order) --\n");
    for r in layout_rows(key_fields) {
        out.push_str(&format!("{r}\n"));
    }

    if !row_fields.is_empty() {
        out.push_str("  -- payload TLV (tag u8 + len u32 BE + value) --\n");
        let mut off = 0usize;
        for f in row_fields {
            let width_note = if f.ty == FieldType::Str {
                "var".to_string()
            } else {
                f.width.to_string()
            };
            out.push_str(&format!(
                "  {:<16} {:>7} {:>6}  {:?}   (frame header at {off})\n",
                f.name,
                off + 5,
                width_note,
                f.ty
            ));
            // Str stride is dynamic (the frame's own len); show the static
            // part (5-byte header) so the column stays aligned.
            off += 5 + if f.ty == FieldType::Str { 0 } else { f.width };
        }
        out.push_str(&format!("  payload total: {off}+ (variable-length frames add their value bytes)\n"));
    }
    out
}

impl<S: KvEngine, K: KeyEncode, R: Row<Key = K>> Table<S, K, R> {
    /// Layout audit for this table's key + row declaration (see [`describe`]).
    pub fn describe(&self) -> String {
        describe::<K, R>()
    }

    /// JSON Schema of this table's snapshot shape — the same column set the
    /// Parquet export writes (key fields first, then payload fields, in
    /// declaration order), so external tools reading the Parquet file can
    /// derive their schema from this instead of introspecting the file.
    pub fn json_schema(&self) -> String {
        json_schema::<K, R>()
    }
}

/// JSON Schema for the exported row shape (see [`Table::json_schema`]).
/// Column type mapping mirrors the Arrow bridge: fixed-width unsigned
/// integers → their JSON number types, `[u8; N]` → base64 string (the same
/// encoding the bridge uses for binary columns). Column order = Parquet
/// column order.
pub fn json_schema<K: KeyEncode, R: Row<Key = K>>() -> String {
    fn json_type(ty: FieldType) -> &'static str {
        match ty {
            FieldType::U8 | FieldType::U16 | FieldType::U32 | FieldType::U64 => "integer",
            FieldType::FixedBytes => "string", // base64, matches Arrow BinaryArray
            FieldType::Str => "string",        // UTF-8, matches Arrow Utf8Array
        }
    }

    let key_fields = <K as KeyEncode>::FIELDS;
    let row_fields = <R as Row>::FIELDS;
    let mut out = String::from(
        r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "properties": {
"#,
    );
    let props: Vec<String> = key_fields
        .iter()
        .chain(row_fields.iter())
        .map(|f| {
            format!(
                "    \"{}\":{{ \"type\": \"{}\", \"description\": \"{} width, {:?} (declaration order)\" }}",
                f.name,
                json_type(f.ty),
                f.width,
                f.ty,
            )
        })
        .collect();
    out.push_str(&props.join(",\n"));
    out.push_str("\n  }");
    // JSON objects are unordered; the authoritative Parquet column order
    // (key fields then payload fields, declaration order) goes here.
    let order: Vec<String> = key_fields
        .iter()
        .chain(row_fields.iter())
        .map(|f| format!("    \"{}\"", f.name))
        .collect();
    out.push_str(&format!(
        ",\n  \"x-okm-column-order\": [\n{}\n  ]\n}}\n",
        order.join(",\n")
    ));
    out
}

/// Parquet snapshot tier (ADR-0007 Phase 3): batch → file, file → store.
#[cfg(feature = "parquet")]
pub mod parquet_io {
    use super::*;
    use arrow::array::{Array, BinaryArray, RecordBatch};

    /// Export all rows to a Parquet file (overwrite). Typed columns, schema
    /// from the declaration — the same batch shape as
    /// [`Table::to_record_batch`].
    pub fn export_parquet<S: KvEngine, K: KeyEncode, R: Row<Key = K>>(
        table: &Table<S, K, R>,
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        let batch = table.to_record_batch();
        let file = std::fs::File::create(path)?;
        let mut w = parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None)?;
        w.write(&batch)?;
        w.close()?;
        Ok(())
    }

    /// Read one row's value from column `col` at `row` as the BE wire bytes
    /// the key encoding / TLV frames expect (reverses the export-side LE
    /// conversion).
    fn wire_bytes(col: &dyn Array, row: usize, width: usize, ty: FieldType) -> Vec<u8> {
        match ty {
            FieldType::FixedBytes => {
                let col = col.as_any().downcast_ref::<BinaryArray>().unwrap();
                col.value(row).to_vec()
            }
            FieldType::Str => {
                let col = col.as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
                col.value(row).as_bytes().to_vec()
            }
            other => {
                let raw: Vec<u8> = match other {
                    FieldType::U8 => vec![col.as_any().downcast_ref::<arrow::array::UInt8Array>().unwrap().value(row)],
                    FieldType::U16 => col
                        .as_any()
                        .downcast_ref::<arrow::array::UInt16Array>()
                        .unwrap()
                        .value(row)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::U32 => col
                        .as_any()
                        .downcast_ref::<arrow::array::UInt32Array>()
                        .unwrap()
                        .value(row)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::U64 => col
                        .as_any()
                        .downcast_ref::<arrow::array::UInt64Array>()
                        .unwrap()
                        .value(row)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::FixedBytes => unreachable!(),
                    FieldType::Str => unreachable!("handled above"),
                };
                debug_assert_eq!(raw.len(), width, "width mismatch on import");
                raw
            }
        }
    }

    /// Import rows from a Parquet file previously written by
    /// [`export_parquet`], writing each row back through [`Table::put`]
    /// (primary key + index entries — the normal write contract). This is
    /// the restore path, not a second write channel.
    ///
    /// Returns the number of rows restored.
    pub fn import_parquet<S: KvEngine, K: KeyEncode, R: Row<Key = K>>(
        table: &mut Table<S, K, R>,
        path: &std::path::Path,
    ) -> parquet::errors::Result<usize> {
        let key_fields = <K as KeyEncode>::FIELDS;
        let row_fields = <R as Row>::FIELDS;
        let nkey = key_fields.len();

        let file = std::fs::File::open(path)?;
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
            .build()?;

        let mut count = 0usize;
        for batch in reader {
            let batch: RecordBatch = batch?;
            let n = batch.num_rows();

            // Reassemble per-row: key encoding (declaration-order BE fields)
            // then the TLV payload (tag + len + value per field).
            let mut keys: Vec<Vec<u8>> = vec![Vec::with_capacity(K::KEY_LEN); n];
            for (fi, f) in key_fields.iter().enumerate() {
                let col = batch.column(fi);
                for (row, krow) in keys.iter_mut().enumerate() {
                    krow.extend_from_slice(&wire_bytes(col, row, f.width, f.ty));
                }
            }
            let mut payloads: Vec<Vec<u8>> = vec![Vec::new(); n];
            for (fi, f) in row_fields.iter().enumerate() {
                let col = batch.column(nkey + fi);
                for (row, prow) in payloads.iter_mut().enumerate() {
                    let wb = wire_bytes(col, row, f.width, f.ty);
                    prow.push(fi as u8);
                    // Variable-length fields: the frame len is the actual
                    // byte length; fixed-width fields re-emit the declared
                    // width (redundant consistency check, same as export).
                    prow.extend_from_slice(&(wb.len() as u32).to_be_bytes());
                    prow.extend_from_slice(&wb);
                }
            }
            for row in 0..n {
                let key = K::decode(&keys[row]);
                let rv = R::decode_payload(&payloads[row]);
                table.put(&key, &rv);
                count += 1;
            }
        }
        Ok(count)
    }
}

