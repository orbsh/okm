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

use crate::model::field::FieldType;
use crate::model::index::Document;
use crate::model::key::KeyEncode;

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
            FieldType::Vocab(_) => "string",   // the label (ADR-0032 export)
            FieldType::Offset(_) => "integer", // logical: base + offset
            FieldType::Vector { .. } => "array", // element sequence (dynamic-length)
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
    use std::sync::Arc;
    use super::*;
    use crate::engine::storage::VirtualStorage;
    use crate::model::document::Collection;
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

    /// Export with the dynamic segment as a `variant` column (ADR-0007's
    /// analytics twin of the dynamic path). The declared fields stay typed
    /// columns; rows without a dynamic segment get `null` in that column.
    ///
    /// This is the export a BACKUP wants: without it the file holds the typed
    /// half of every row and silently loses the open half.
    pub fn export_parquet_with_variant<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>>(
        collection: &mut Collection<S, K, R>,
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        let batch = collection.to_record_batch_with_variant();
        let file = std::fs::File::create(path)?;
        let mut w = parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None)?;
        w.write(&batch)?;
        w.close()?;
        Ok(())
    }

    /// Write an already-built batch to a Parquet file — the seam for exports
    /// whose rows come from somewhere other than a collection (the graph's edge
    /// table, the catalog). One writer, so every file in a backup has the same
    /// provenance.
    pub fn write_batch(
        batch: &arrow::array::RecordBatch,
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut w = parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None)?;
        w.write(batch)?;
        w.close()?;
        Ok(())
    }

    /// The CATALOG (ADR-0034 §1): one row per table, `(name, ns)`. Pure analysis
    /// does not read it — the exported columns are already labeled — but an import
    /// needs the name→ns direction, and so does anyone asking "what is ns 41 called".
    /// It travels as its own file, beside the tables it names.
    pub fn catalog_parquet(
        tables: &[(&str, u16)],
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        use arrow::array::{Array, RecordBatch, UInt16Array};
        use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};

        let mut names = arrow::array::StringBuilder::new();
        for (name, _) in tables {
            names.append_value(*name);
        }
        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("table", DataType::Utf8, false),
            ArrowField::new("ns", DataType::UInt16, false),
        ]));
        let arrays: Vec<Arc<dyn Array>> = vec![
            Arc::new(names.finish()),
            Arc::new(UInt16Array::from(
                tables.iter().map(|(_, ns)| *ns).collect::<Vec<u16>>(),
            )),
        ];
        let batch = RecordBatch::try_new(schema, arrays)?;
        write_batch(&batch, path)
    }

    /// The DERIVED reduce table (ADR-0034): one row per reduce entry —
    /// `(table, reduce, group, value)`.
    ///
    /// Reduces are not rows: they live in their own slot family and hold an
    /// accumulator, not a document. They still have to travel with a backup,
    /// because a restored table whose watermark is missing hands out ids it
    /// already used. One row per entry keeps the shape ordinary: a table name, the
    /// reduce's slot, the group bytes (empty = the whole table), and the value.
    pub fn export_reduces<S: VirtualStorage, A: crate::model::reduce::Reduce>(
        store: &S,
        ns_prefix: &[u8],
        table: &str,
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        use arrow::array::{Array, BinaryArray, UInt16Array};
        use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
        use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};

        let mut prefix = ns_prefix.to_vec();
        crate::model::index::write_slot(&mut prefix, A::SLOT);
        let entries = crate::engine::storage::scan_suffix_kv(store, &prefix);

        let binary = |vals: &[Vec<u8>]| -> BinaryArray {
            let mut offsets: Vec<i32> = Vec::with_capacity(vals.len() + 1);
            offsets.push(0);
            let mut data = Vec::new();
            for v in vals {
                data.extend_from_slice(v);
                offsets.push(data.len() as i32);
            }
            BinaryArray::new(
                OffsetBuffer::new(ScalarBuffer::from(offsets)),
                Buffer::from(data),
                None,
            )
        };

        let tables: Vec<Option<String>> = entries.iter().map(|_| Some(table.to_string())).collect();
        let mut t_builder = arrow::array::StringBuilder::new();
        for t in &tables {
            t_builder.append_option(t.clone());
        }
        let slots: Vec<u16> = entries.iter().map(|_| A::SLOT).collect();
        let groups: Vec<Vec<u8>> = entries.iter().map(|(suffix, _)| suffix.clone()).collect();
        let values: Vec<Vec<u8>> = entries.iter().map(|(_, value)| value.clone()).collect();

        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("table", DataType::Utf8, false),
            ArrowField::new("reduce", DataType::UInt16, false),
            ArrowField::new("group", DataType::Binary, false),
            ArrowField::new("value", DataType::Binary, false),
        ]));
        let arrays: Vec<Arc<dyn Array>> = vec![
            Arc::new(t_builder.finish()),
            Arc::new(UInt16Array::from(slots)),
            Arc::new(binary(&groups)),
            Arc::new(binary(&values)),
        ];
        let batch = arrow::array::RecordBatch::try_new(schema, arrays)?;
        let file = std::fs::File::create(path)?;
        let mut w = parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None)?;
        w.write(&batch)?;
        w.close()?;
        Ok(())
    }

    /// Read one document's value from column `col` at `document` as the BE wire bytes
    /// the key encoding / TLV frames expect (reverses the export-side LE
    /// conversion).
    ///
    /// `Err` is a refusal, never a guess: a `Vocab` label the destination
    /// vocabulary cannot name (a NULL in the column), or a file whose `Vocab`
    /// column is not a label column — the pre-label export carried raw ids and
    /// is NOT readable here (no compatibility path: the vocabulary that would
    /// name those ids is not in the file).
    fn wire_bytes<S: VirtualStorage>(
        col: &dyn Array,
        document: usize,
        field: &crate::model::field::FieldDesc,
        store: &mut S,
        ns_header: &[u8],
    ) -> Result<Vec<u8>, String> {
        use crate::model::wrappers::{VarIntEnc as _, quantize, wire_to_be_bytes};
        let ty = field.ty;
        let width = field.width;
        if let FieldType::Vocab(id_slot) = ty {
            let labels = col
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .ok_or_else(|| {
                    format!(
                        "column `{}` is not a label column — this file predates the label \
                         export (it carries ids, and the vocabulary is not in the file); \
                         re-export it from the source store",
                        field.name
                    )
                })?;
            if labels.is_null(document) {
                return Err(format!(
                    "column `{}` row {}: NULL label — the source row names an id its \
                     vocabulary does not define; refusing to invent one",
                    field.name, document
                ));
            }
            let faces = crate::model::obj_vocab::VocabFaces::from_id_slot(id_slot);
            return crate::model::obj_vocab::vocab_id(
                store,
                ns_header,
                faces,
                width,
                labels.value(document),
            )
            .ok_or_else(|| {
                format!(
                    "column `{}`: no vocabulary id width matches {} byte(s)",
                    field.name, width
                )
            });
        }
        let raw: Vec<u8> = match ty {
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
                    | FieldType::Vocab(_)
                    | FieldType::Offset(_)
                    | FieldType::Vector { .. } => unreachable!("handled above"),
                };
                debug_assert_eq!(raw.len(), width, "width mismatch on import");
                raw
            }
        };
        Ok(raw)
    }

    /// Import documents from a Parquet file previously written by
    /// [`export_parquet`], writing each document back through [`Collection::put`]
    /// (primary key + index entries — the normal write contract). This is
    /// the restore path, not a second write channel.
    ///
    /// Categorical fields arrive as LABELS and are re-interned into THIS
    /// collection's vocabulary before the row bytes are assembled, so the ids in
    /// the restored rows are the destination's own numbering — the snapshot
    /// carries names, not codes. A file from the pre-label export (raw id
    /// columns) is refused rather than misread.
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

            // Vocabulary keys live under the same `[part][ns]` header the
            // intern hook writes through.
            let mut ns_header = <R as Document>::PARTITION_PREFIX.to_vec();
            ns_header.extend_from_slice(<R as Document>::NS_PREFIX);

            // Phase 1 — reassemble per-document bytes: key encoding
            // (declaration-order BE fields) then the two-segment payload —
            // [ver u8][hot_len u16 BE][hot segment][cold TLV] — matching the
            // export-side layout. Labels intern here, so this phase borrows the
            // store mutably; it ends before the rows land.
            let (keys, payloads) = {
                let store = collection.store_mut();
                let mut keys: Vec<Vec<u8>> = vec![Vec::with_capacity(K::KEY_LEN); n];
                for (fi, f) in key_fields.iter().enumerate() {
                    let col = batch.column(fi);
                    for (document, krow) in keys.iter_mut().enumerate() {
                        let wb = wire_bytes(col, document, f, store, &ns_header)
                            .map_err(parquet::errors::ParquetError::General)?;
                        krow.extend_from_slice(&wb);
                    }
                }
                let hot_width: usize =
                    row_fields.iter().filter(|f| f.width > 0).map(|f| f.width).sum();
                let ver = <R as Document>::LAYOUT_VERSION;
                let pay_header = vec![ver, (hot_width >> 8) as u8, hot_width as u8];
                let mut payloads: Vec<Vec<u8>> = vec![pay_header; n];
                // Hot segment: contiguous fixed-width runs (no frame headers).
                for (fi, f) in row_fields.iter().enumerate() {
                    if f.width == 0 {
                        continue;
                    }
                    let col = batch.column(nkey + fi);
                    for (document, prow) in payloads.iter_mut().enumerate() {
                        let wb = wire_bytes(col, document, f, store, &ns_header)
                            .map_err(parquet::errors::ParquetError::General)?;
                        prow.extend_from_slice(&wb);
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
                        let wb = wire_bytes(col, document, f, store, &ns_header)
                            .map_err(parquet::errors::ParquetError::General)?;
                        prow.push(fi as u8);
                        // P2 wire: frame length is the prefix-monotonic varint
                        // (shared codec — one length discipline everywhere).
                        crate::put_len(prow, wb.len());
                        prow.extend_from_slice(&wb);
                    }
                }
                (keys, payloads)
            };

            // Phase 2 — the normal write contract (primary + indexes + faces).
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

