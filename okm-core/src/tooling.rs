//! Tooling interfaces (PLAN Phase 4): layout audit tables, Parquet snapshot
//! export/import.
//!
//! These are library functions, not a standalone binary: `Collection<S, K, R>`
//! binds user types at compile time, so the type context must live in the
//! caller's application. A "CLI experience" is a thin bin wrapping these
//! calls.
//!
//! - `json_schema` renders the declaration-order byte layout as
//!   machine-readable JSON (from the same `FieldDesc` tables the derive
//!   macros emit — the declaration is still the single source; nothing
//!   re-parses source code).
//! - `export_parquet` / `import_parquet` (feature `parquet`) sit on the
//!   Arrow bridge: RecordBatch → Parquet file, and back. Import writes
//!   through [`Collection::put`] — the normal one-batch write contract (primary
//!   key + index entries) is never bypassed; this is a backup/restore path,
//!   not a second write channel.

use crate::field::FieldType;
use crate::index::Document;
use crate::key::KeyEncode;

/// Declaration-order offset table for a fixed-width region (the key
/// encoding, or the value half of TLV frames — widths are declared, so the
/// stride math is the same for both).
/// JSON Schema for the exported document shape (see [`Collection::json_schema`]).
/// Column type mapping mirrors the Arrow bridge: fixed-width unsigned
/// integers → their JSON number types, `[u8; N]` → base64 string (the same
/// encoding the bridge uses for binary columns). Column order = Parquet
/// column order.
pub fn json_schema<K: KeyEncode, R: Document<Key = K>>() -> String {
    fn json_type(ty: FieldType) -> &'static str {
        match ty {
            FieldType::U8 | FieldType::U16 | FieldType::U32 | FieldType::U64 => "integer",
            FieldType::FixedBytes => "string", // base64, matches Arrow BinaryArray
            FieldType::Str => "string",        // UTF-8, matches Arrow Utf8Array
            FieldType::Bytes => "string",      // base64, matches Arrow BinaryArray
            FieldType::VarInt => "integer",    // logical: the decoded integer
            FieldType::Quant(_) => "number",   // logical: dequantized f64
            FieldType::Enum => "integer",      // the u8 tag
            FieldType::Offset(_) => "integer", // logical: base + offset
        }
    }

    let key_fields = <K as KeyEncode>::FIELDS;
    let row_fields = <R as Document>::FIELDS;
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
    use crate::storage::VirtualStorage;
    use crate::document::Collection;
    use arrow::array::{Array, BinaryArray, RecordBatch};

    /// Export all documents to a Parquet file (overwrite). Typed columns, schema
    /// from the declaration — the same batch shape as
    /// [`Collection::to_record_batch`].
    pub fn export_parquet<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>>(
        collection: &Collection<S, K, R>,
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        let batch = collection.to_record_batch();
        let file = std::fs::File::create(path)?;
        let mut w = parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None)?;
        w.write(&batch)?;
        w.close()?;
        Ok(())
    }

    /// Read one document's value from column `col` at `document` as the BE wire bytes
    /// the key encoding / TLV frames expect (reverses the export-side LE
    /// conversion).
    fn wire_bytes(col: &dyn Array, document: usize, width: usize, ty: FieldType) -> Vec<u8> {
        use crate::wrappers::{VarIntEnc as _, quantize, wire_to_be_bytes};
        match ty {
            FieldType::FixedBytes => {
                let col = col.as_any().downcast_ref::<BinaryArray>().unwrap();
                col.value(document).to_vec()
            }
            FieldType::Str => {
                let col = col.as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
                col.value(document).as_bytes().to_vec()
            }
            FieldType::VarInt => {
                let c = col.as_any().downcast_ref::<arrow::array::UInt64Array>().unwrap();
                let mut buf = Vec::with_capacity(10);
                c.value(document).varint_encode(&mut buf);
                buf
            }
            FieldType::Quant(p) => {
                // Re-quantize from the dequantized f64 column value at the
                // declared precision; wire is the fixed-point i64 BE.
                let c = col.as_any().downcast_ref::<arrow::array::Float64Array>().unwrap();
                wire_to_be_bytes(quantize(c.value(document), p))
            }
            FieldType::Enum => {
                let c = col.as_any().downcast_ref::<arrow::array::UInt8Array>().unwrap();
                vec![c.value(document)]
            }
            FieldType::Offset(base) => {
                let c = col.as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
                crate::offset_encode(c.value(document), base)
            }
            other => {
                let raw: Vec<u8> = match other {
                    FieldType::U8 => vec![col.as_any().downcast_ref::<arrow::array::UInt8Array>().unwrap().value(document)],
                    FieldType::U16 => col
                        .as_any()
                        .downcast_ref::<arrow::array::UInt16Array>()
                        .unwrap()
                        .value(document)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::U32 => col
                        .as_any()
                        .downcast_ref::<arrow::array::UInt32Array>()
                        .unwrap()
                        .value(document)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::U64 => col
                        .as_any()
                        .downcast_ref::<arrow::array::UInt64Array>()
                        .unwrap()
                        .value(document)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::FixedBytes => unreachable!(),
                    FieldType::Bytes => {
                        col.as_any()
                            .downcast_ref::<arrow::array::BinaryArray>()
                            .unwrap()
                            .value(document)
                            .to_vec()
                    }
                    FieldType::Str => unreachable!("handled above"),
                    FieldType::VarInt
                    | FieldType::Quant(_)
                    | FieldType::Enum
                    | FieldType::Offset(_) => unreachable!("handled above"),
                };
                debug_assert_eq!(raw.len(), width, "width mismatch on import");
                raw
            }
        }
    }

    /// Import documents from a Parquet file previously written by
    /// [`export_parquet`], writing each document back through [`Collection::put`]
    /// (primary key + index entries — the normal write contract). This is
    /// the restore path, not a second write channel.
    ///
    /// Returns the number of documents restored.
    pub fn import_parquet<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>>(
        collection: &mut Collection<S, K, R>,
        path: &std::path::Path,
    ) -> parquet::errors::Result<usize> {
        let key_fields = <K as KeyEncode>::FIELDS;
        let row_fields = <R as Document>::FIELDS;
        let nkey = key_fields.len();

        let file = std::fs::File::open(path)?;
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
            .build()?;

        let mut count = 0usize;
        for batch in reader {
            let batch: RecordBatch = batch?;
            let n = batch.num_rows();

            // Reassemble per-document: key encoding (declaration-order BE fields)
            // then the two-segment payload — [ver u8][hot_len u16 BE]
            // [hot segment][cold TLV] — matching the export-side layout.
            let mut keys: Vec<Vec<u8>> = vec![Vec::with_capacity(K::KEY_LEN); n];
            for (fi, f) in key_fields.iter().enumerate() {
                let col = batch.column(fi);
                for (document, krow) in keys.iter_mut().enumerate() {
                    krow.extend_from_slice(&wire_bytes(col, document, f.width, f.ty));
                }
            }
            let hot_width: usize = row_fields.iter().filter(|f| f.width > 0).map(|f| f.width).sum();
            let ver = <R as Document>::LAYOUT_VERSION;
            let header = vec![ver, (hot_width >> 8) as u8, hot_width as u8];
            let mut payloads: Vec<Vec<u8>> = vec![header; n];
            // Hot segment: contiguous fixed-width runs (no frame headers).
            for (fi, f) in row_fields.iter().enumerate() {
                if f.width == 0 {
                    continue;
                }
                let col = batch.column(nkey + fi);
                for (document, prow) in payloads.iter_mut().enumerate() {
                    prow.extend_from_slice(&wire_bytes(col, document, f.width, f.ty));
                }
            }
            // Cold segment: one TLV frame per variable-length field, tag =
            // the field's declaration index.
            for (fi, f) in row_fields.iter().enumerate() {
                if f.width > 0 {
                    continue;
                }
                let col = batch.column(nkey + fi);
                for (document, prow) in payloads.iter_mut().enumerate() {
                    let wb = wire_bytes(col, document, f.width, f.ty);
                    prow.push(fi as u8);
                    prow.extend_from_slice(&(wb.len() as u32).to_be_bytes());
                    prow.extend_from_slice(&wb);
                }
            }
            for document in 0..n {
                let key = K::decode(&keys[document]);
                let rv = R::decode_payload(&payloads[document]);
                collection.put(&key, &rv);
                count += 1;
            }
        }
        Ok(count)
    }
}

