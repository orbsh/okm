//! The snapshot half of the format work: a table's CURRENT STATE as Parquet
//! (`export_parquet`, `export_parquet_with_variant`, `export_reduces`) and back
//! (`import_parquet`), plus the JSON-schema view of a declaration.
//!
//! State, not history: a deleted row is simply absent, because this reads the store.
//! Change export is the log's job (ADR-0045 §8) — a window carries removals and a
//! columnar append cannot.
//!
//! It lives in the drain crate for the same reason `bridge` does: Parquet is a
//! format, and the store does not need it to be a store. The graph's restore half is
//! the `EdgeRestore` trait, for the same cross-crate reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use crate::bridge::ToRecordBatch;
use okm_core::model::field::FieldType;
use okm_core::model::hyper::{Decl, Fragment};
use okm_core::model::index::Document;
use okm_core::model::key::KeyEncode;
use okm_value::{Dict, Variant};

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
            FieldType::Uuid4 | FieldType::Uuid7 => "string", // hyphenated text, 16 bytes on the wire
            FieldType::Str => "string",                      // UTF-8, matches Arrow Utf8Array
            FieldType::Bytes => "string",                    // base64, matches Arrow BinaryArray
            FieldType::VarInt => "integer",                  // logical: the decoded integer
            FieldType::Quant(_) => "number",                 // logical: dequantized f64
            FieldType::Vocab(_) => "string",                 // the label (ADR-0032 export)
            FieldType::Offset(_) => "integer",               // logical: base + offset
            FieldType::Vector(_) => "array",                 // element sequence (dynamic-length)
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
pub mod parquet_io {
    use super::*;
    use arrow::array::{Array, BinaryArray};
    use okm_core::engine::storage::VirtualStorage;
    use okm_core::model::document::Collection;
    use std::sync::Arc;

    /// Export all documents to a Parquet file (overwrite). Typed columns, schema
    /// from the declaration — the same batch shape as
    /// [`Collection::to_record_batch`].
    pub fn export_parquet<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>>(
        collection: &Collection<S, K, R>,
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        let batch = collection
            .to_record_batch()
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        write_batch(&batch, path)
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
        let batch = collection
            .to_record_batch_with_variant()
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        write_batch(&batch, path)
    }

    /// The same table as Parquet bytes: a lake in a bucket has no path to write to.
    pub fn export_parquet_with_variant_bytes<
        S: VirtualStorage,
        K: KeyEncode,
        R: Document<Key = K>,
    >(
        collection: &mut Collection<S, K, R>,
    ) -> parquet::errors::Result<Vec<u8>> {
        let batch = collection
            .to_record_batch_with_variant()
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        batch_bytes(&batch)
    }

    /// A batch as Parquet BYTES — what a lake that is not a directory is handed. The same
    /// writer as every other export, so a published file and a folded one cannot disagree
    /// about the format.
    pub fn batch_bytes(batch: &arrow::array::RecordBatch) -> parquet::errors::Result<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let mut w = parquet::arrow::ArrowWriter::try_new(&mut out, batch.schema(), None)?;
        w.write(batch)?;
        w.close()?;
        Ok(out)
    }

    /// Write an already-built batch to a Parquet file — the seam for exports
    /// whose rows come from somewhere other than a collection (the graph's edge
    /// table, the catalog). One writer, so every file in a backup has the same
    /// provenance.
    pub fn write_batch(
        batch: &arrow::array::RecordBatch,
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        // Columns are identified by NAME: the metadata is a Delta log
        // (`okm_lake::delta`), which records a schema by name, and a Parquet
        // field id would be a second identity nothing reads.
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
    pub fn export_reduces<S: VirtualStorage, A: okm_core::model::reduce::Reduce>(
        store: &S,
        ns_prefix: &[u8],
        table: &str,
        path: &std::path::Path,
    ) -> parquet::errors::Result<()> {
        let batch = reduces_batch::<S, A>(store, ns_prefix, table)?;
        write_batch(&batch, path)
    }

    /// The same reduce table as bytes, for a lake that is a bucket.
    pub fn export_reduces_bytes<S: VirtualStorage, A: okm_core::model::reduce::Reduce>(
        store: &S,
        ns_prefix: &[u8],
        table: &str,
    ) -> parquet::errors::Result<Vec<u8>> {
        let batch = reduces_batch::<S, A>(store, ns_prefix, table)?;
        batch_bytes(&batch)
    }

    /// A reduce declaration's entries as the derived table `(table, reduce, group, value)`.
    fn reduces_batch<
        S: okm_core::engine::storage::VirtualStorage,
        A: okm_core::model::reduce::Reduce,
    >(
        store: &S,
        ns_prefix: &[u8],
        table: &str,
    ) -> parquet::errors::Result<arrow::array::RecordBatch> {
        use arrow::array::{Array, BinaryArray, UInt16Array};
        use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
        use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};
        use std::sync::Arc;

        let mut prefix = ns_prefix.to_vec();
        okm_core::model::index::write_slot(&mut prefix, A::SLOT);
        let entries = okm_core::engine::storage::scan_suffix_kv(store, &prefix)
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;

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
        Ok(arrow::array::RecordBatch::try_new(schema, arrays)?)
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
    /// Downcast one column of a parquet file written by SOMETHING ELSE. A column
    /// whose arrow type does not match the declared field is an input error the
    /// caller must hear about — unlike the codec's own decode, whose bytes can only
    /// have come from this SDK (ADR-0040).
    fn column_as<'a, T: 'static>(col: &'a dyn Array, field_name: &str) -> Result<&'a T, String> {
        col.as_any().downcast_ref::<T>().ok_or_else(|| {
            format!(
                "column `{field_name}`: the file's arrow type is not the one this field declares"
            )
        })
    }

    pub(crate) fn wire_bytes<S: VirtualStorage>(
        col: &dyn Array,
        document: usize,
        field: &okm_core::model::field::FieldDesc,
        store: &mut S,
        ns_header: &[u8],
    ) -> Result<Vec<u8>, String> {
        use okm_core::model::wrappers::{VarIntEnc as _, quantize, wire_to_be_bytes};
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
            let faces = okm_core::model::obj_vocab::VocabFaces::from_id_slot(id_slot);
            return okm_core::model::obj_vocab::vocab_id(
                store,
                ns_header,
                faces,
                width,
                labels.value(document),
            )
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "column `{}`: no vocabulary id width matches {} byte(s)",
                    field.name, width
                )
            });
        }
        let raw: Vec<u8> = match ty {
            FieldType::FixedBytes => {
                let col = column_as::<BinaryArray>(col, field.name)?;
                col.value(document).to_vec()
            }
            // 16 fixed bytes back from the warehouse column, byte order untouched.
            FieldType::Uuid4 | FieldType::Uuid7 => {
                let col = column_as::<arrow::array::FixedSizeBinaryArray>(col, field.name)?;
                col.value(document).to_vec()
            }
            FieldType::Str => {
                let col = column_as::<arrow::array::StringArray>(col, field.name)?;
                col.value(document).as_bytes().to_vec()
            }
            FieldType::VarInt => {
                let c = column_as::<arrow::array::UInt64Array>(col, field.name)?;
                let mut buf = Vec::with_capacity(10);
                c.value(document).varint_encode(&mut buf);
                buf
            }
            FieldType::Quant(p) => {
                // Re-quantize from the dequantized f64 column value at the
                // declared precision; wire is the fixed-point i64 BE.
                let c = column_as::<arrow::array::Float64Array>(col, field.name)?;
                wire_to_be_bytes(quantize(c.value(document), p))
            }
            FieldType::Offset(base) => {
                let c = column_as::<arrow::array::Int64Array>(col, field.name)?;
                okm_core::offset_encode(c.value(document), base)
            }
            // A `Vector` exports as the OPAQUE frame bytes (the bridge keeps the
            // element type in the schema and the column byte-opaque), so the
            // import is the same read: the bytes go back verbatim.
            FieldType::Vector(_) => {
                let col = column_as::<BinaryArray>(col, field.name)?;
                col.value(document).to_vec()
            }
            other => {
                let raw: Vec<u8> = match other {
                    FieldType::U8 => vec![
                        column_as::<arrow::array::UInt8Array>(col, field.name)?.value(document),
                    ],
                    FieldType::U16 => column_as::<arrow::array::UInt16Array>(col, field.name)?
                        .value(document)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::U32 => column_as::<arrow::array::UInt32Array>(col, field.name)?
                        .value(document)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::U64 => column_as::<arrow::array::UInt64Array>(col, field.name)?
                        .value(document)
                        .to_be_bytes()
                        .to_vec(),
                    FieldType::FixedBytes | FieldType::Uuid4 | FieldType::Uuid7 => unreachable!(),
                    FieldType::Bytes => column_as::<arrow::array::BinaryArray>(col, field.name)?
                        .value(document)
                        .to_vec(),
                    FieldType::Str => unreachable!("handled above"),
                    FieldType::VarInt
                    | FieldType::Quant(_)
                    | FieldType::Vocab(_)
                    | FieldType::Offset(_)
                    | FieldType::Vector(_) => unreachable!("handled above"),
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
        let file = std::fs::File::open(path)?;
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
            .build()?;
        let mut count = 0usize;
        for batch in reader {
            count += import_parquet_batch(collection, &batch?)?;
        }
        Ok(count)
    }

    /// A Parquet file's batches, in memory — what a lake hands over: a producer
    /// publishes bytes, so a restore must be able to read bytes (ADR-0062).
    pub fn read_batches(
        bytes: bytes::Bytes,
    ) -> parquet::errors::Result<Vec<arrow::array::RecordBatch>> {
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(bytes)?
            .build()?;
        let mut batches = Vec::new();
        for batch in reader {
            batches.push(batch?);
        }
        Ok(batches)
    }

    /// ONE batch into a collection — the body of [`Self::import_parquet`], so a
    /// caller holding bytes (a lake) never routes them through a file.
    ///
    /// The collection decides the ns: `Collection::with_ns` roots the same
    /// declaration at another namespace, and the vocabulary header this rebuilds
    /// follows it, so a restore can map a table into a namespace the application
    /// just created (ADR-0062).
    pub fn import_parquet_batch<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>>(
        collection: &mut Collection<S, K, R>,
        batch: &arrow::array::RecordBatch,
    ) -> parquet::errors::Result<usize> {
        let key_fields = <K as KeyEncode>::FIELDS;
        let row_fields = <R as Document>::FIELDS;
        let nkey = key_fields.len();
        let n = batch.num_rows();
        // Vocabulary keys live under the same `[part][ns]` header the intern hook
        // writes through — the COLLECTION's, not the declaration's.
        let ns_header = collection.header();
        let mut count = 0usize;
        // Phase 1 — reassemble per-document bytes: key encoding
        // (declaration-order BE fields) then the two-segment payload —
        // [hot segment][cold TLV] — matching the
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
            // No payload header: the hot width is the declaration's, so an
            // imported row starts straight at its first field (ADR-0042).
            let mut payloads: Vec<Vec<u8>> = vec![Vec::new(); n];
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
                    okm_core::put_len(prow, wb.len());
                    prow.extend_from_slice(&wb);
                }
            }
            (keys, payloads)
        };

        // Phase 2 — the normal write contract (primary + indexes + faces),
        // plus the dynamic segment the `variant` column carries. A row whose
        // payload columns are ALL null is a segment-only document (ADR-0035's
        // pair of slots): it imports as a segment and NO row, instead of an
        // invented default payload the export never had.
        let variant_idx = batch
            .schema()
            .fields()
            .iter()
            .position(|f| f.name() == "variant");
        let payload_cols: Vec<usize> = (nkey..batch.num_columns())
            .filter(|i| Some(*i) != variant_idx)
            .collect();
        for document in 0..n {
            let key = K::decode(&keys[document]);
            // No payload columns at all (a fieldless row) cannot be told
            // apart from a segment-only document here, so the row is written:
            // an empty payload is the only thing a fieldless table exports.
            let has_row = payload_cols.is_empty()
                || payload_cols
                    .iter()
                    .any(|i| !batch.column(*i).is_null(document));
            if has_row {
                let rv = R::decode_payload(&payloads[document]);
                collection
                    .put(&key, &rv)
                    .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
                count += 1;
            }
            let segment = match variant_idx {
                Some(vi) => segment_from_variant(batch.column(vi), document)
                    .map_err(parquet::errors::ParquetError::General)?,
                None => None,
            };
            if let Some(map) = segment {
                // Segment names are by construction non-declared, so this
                // merge writes the segment and leaves the row alone. A
                // declared name would come back as an error rather than a
                // silent default — the import path propagates it.
                collection
                    .put(&key, &Variant::Map(map))
                    .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
            }
        }
        Ok(count)
    }

    /// One document's dynamic segment out of its `variant` cell:
    /// `struct<metadata, value>` → the name-keyed map the segment stores. The
    /// export's `push_variant` written backwards; names come back resolved because
    /// the Parquet Variant's own metadata carries them.
    fn segment_from_variant(
        col: &dyn arrow::array::Array,
        row: usize,
    ) -> Result<Option<Dict>, String> {
        if col.is_null(row) {
            return Ok(None);
        }
        use arrow::array::BinaryArray;
        let arr = col
            .as_any()
            .downcast_ref::<arrow::array::StructArray>()
            .ok_or_else(|| "the variant column is not a struct".to_string())?;
        let half = |name: &str| -> Result<&BinaryArray, String> {
            arr.column_by_name(name)
                .and_then(|c| c.as_any().downcast_ref::<BinaryArray>())
                .ok_or_else(|| format!("a variant cell has no `{name}` half"))
        };
        let variant = parquet_variant::Variant::try_new(
            half("metadata")?.value(row),
            half("value")?.value(row),
        )
        .map_err(|e| e.to_string())?;
        match dynamic_from_variant(variant) {
            okm_value::Variant::Map(map) => Ok(Some(map)),
            _ => Err("a document's variant cell holds an object, not a scalar".into()),
        }
    }

    /// Parquet Variant → okm `Variant`. Integers come back SIGNED (the export widens UInt to
    /// i64, a documented limitation), so a non-negative value is restored as `UInt`
    /// and a negative one as `Int` — the export cannot tell them apart.
    fn dynamic_from_variant(v: parquet_variant::Variant<'_, '_>) -> okm_value::Variant {
        use okm_value::Variant as DV;
        if v.as_null().is_some() {
            return DV::Null;
        }
        if let Some(b) = v.as_boolean() {
            return DV::Bool(b);
        }
        if let Some(s) = v.as_string() {
            return DV::Str(s.to_string());
        }
        if let Some(u) = v.as_u64() {
            return DV::U64(u);
        }
        if let Some(i) = v.as_int64() {
            return DV::I64(i);
        }
        if let Some(f) = v.as_f64() {
            return DV::F64(f);
        }
        if let Some(bytes) = v.as_u8_slice() {
            return DV::Bytes(bytes.to_vec());
        }
        if let Some(obj) = v.as_object() {
            return DV::Map(
                obj.iter()
                    .map(|(name, field)| (name.to_string(), dynamic_from_variant(field)))
                    .collect(),
            );
        }
        if let Some(list) = v.as_list() {
            return DV::Array(list.iter().map(dynamic_from_variant).collect());
        }
        DV::Null
    }

    /// The restore half of [`Graph::to_record_batch_table`]: write an edge table
    /// back through the graph's own write path (`link_into` + `replace_into`,
    /// one commit per edge), so every derived face entry follows and no second
    /// write channel appears.
    ///
    /// The endpoints arrive as `(table name, raw key)` pairs — one edge table spans
    /// node collections, so the key cannot be decoded without the declaration of
    /// the collection it belongs to; `ns_of` turns a name into that collection's
    /// `[ns 2B]` header. An unknown name is a REFUSAL naming the row, never a
    /// guessed ns. The edge id rides the first key column, so a restore keeps the
    /// ids the backup carries instead of renumbering (§5's rule for watermarks,
    /// applied to the edge sequence).
    /// The restore half of the edge table: write it back through the graph's own
    /// write path, so every derived face follows. A trait because the graph is
    /// `okm-core`'s and this crate is not.
    pub trait EdgeRestore {
        fn import_file<F>(&mut self, path: &std::path::Path, ns_of: F) -> Result<usize, String>
        where
            F: Fn(&str) -> Option<u16>;
        fn import_table<F>(
            &mut self,
            batch: &arrow::array::RecordBatch,
            ns_of: F,
        ) -> Result<usize, String>
        where
            F: Fn(&str) -> Option<u16>;
    }

    impl<
        S: okm_core::engine::storage::VirtualStorage,
        E: okm_core::model::graph::GraphEncode + Default + 'static,
    > EdgeRestore for okm_core::model::graph::Graph<S, E>
    {
        /// The file form of [`Self::import_table`]: read every batch and land it,
        /// so a caller never assembles a batch the export did not hand it. An empty
        /// file restores nothing (0), which is the export of an empty table.
        fn import_file<F>(&mut self, path: &std::path::Path, ns_of: F) -> Result<usize, String>
        where
            F: Fn(&str) -> Option<u16>,
        {
            let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
                    .map_err(|e| format!("{}: {e}", path.display()))?
                    .build()
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            let mut rows = 0usize;
            for batch in reader {
                let batch = batch.map_err(|e| format!("{}: {e}", path.display()))?;
                rows += self.import_table(&batch, &ns_of)?;
            }
            Ok(rows)
        }

        fn import_table<F>(
            &mut self,
            batch: &arrow::array::RecordBatch,
            ns_of: F,
        ) -> Result<usize, String>
        where
            F: Fn(&str) -> Option<u16>,
        {
            use arrow::array::{Array, BinaryArray, StringArray};
            use okm_core::model::graph::{EdgeHead, EdgeKey, NodeRef};

            let key_fields = <EdgeKey as KeyEncode>::FIELDS;
            let row_fields = <E as Document>::FIELDS;
            // Columns are found BY NAME, so a table may carry more than this
            // declaration names: the endpoint pair's typed key columns, for one,
            // which a property-graph declaration needs and a restore ignores.
            let col = |name: &str| -> Result<usize, String> {
                batch.schema().index_of(name).map_err(|_| {
                    format!(
                        "the edge table has no `{name}` column; this graph's declaration needs it"
                    )
                })
            };
            let (c_src_t, c_src_k) = (col("src_table")?, col("src_pkey")?);
            let (c_dst_t, c_dst_k) = (col("dst_table")?, col("dst_pkey")?);
            let c_pred = col("predicate")?;
            let c_variant = col("variant")?;
            let c_key = col(key_fields[0].name)?;
            let c_payload: Vec<usize> = row_fields
                .iter()
                .map(|f| col(f.name))
                .collect::<Result<_, _>>()?;
            let n = batch.num_rows();
            let endpoint =
                |table_col: usize, key_col: usize, row: usize| -> Result<NodeRef, String> {
                    let tables = batch
                        .column(table_col)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or_else(|| format!("column {table_col} is not a table-name column"))?;
                    let keys = batch
                        .column(key_col)
                        .as_any()
                        .downcast_ref::<BinaryArray>()
                        .ok_or_else(|| format!("column {key_col} is not a raw-key column"))?;
                    if tables.is_null(row) {
                        return Err(format!(
                            "row {row}: column {table_col} has no table name — the endpoint's \
                     collection is unknown, and a name is never invented"
                        ));
                    }
                    let name = tables.value(row);
                    let ns = ns_of(name).ok_or_else(|| {
                        format!("row {row}: no collection named `{name}` in this store")
                    })?;
                    Ok(NodeRef::new(&ns.to_be_bytes(), keys.value(row)))
                };

            let mut heads: Vec<EdgeHead> = Vec::with_capacity(n);
            let mut ids: Vec<u64> = Vec::with_capacity(n);
            let mut payloads: Vec<E> = Vec::with_capacity(n);
            let mut props: Vec<Dict> = Vec::with_capacity(n);
            {
                let store = self.store_mut();
                let mut ns_header = <E as Document>::PARTITION_PREFIX.to_vec();
                ns_header.extend_from_slice(<E as Document>::NS_PREFIX);
                for row in 0..n {
                    let src = endpoint(c_src_t, c_src_k, row)?;
                    let dst = endpoint(c_dst_t, c_dst_k, row)?;
                    let predicates = batch
                        .column(c_pred)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or("column 4 is not a predicate column")?;
                    if predicates.is_null(row) {
                        return Err(format!("row {row}: the edge has no predicate"));
                    }
                    // The key half: the edge id, at the key column's own width.
                    let key_bytes =
                        wire_bytes(batch.column(c_key), row, &key_fields[0], store, &ns_header)?;
                    let id_bytes: [u8; 8] = key_bytes.as_slice().try_into().map_err(|_| {
                        format!(
                            "row {row}: edge key is {} bytes, not a u64",
                            key_bytes.len()
                        )
                    })?;
                    // The payload: `[hot segment][cold TLV]` — no header, the hot
                    // width is the declaration's (ADR-0042 as amended) — reassembled
                    // from the typed columns.
                    let mut payload: Vec<u8> = Vec::new();
                    for (fi, f) in row_fields.iter().enumerate() {
                        if f.width == 0 {
                            continue;
                        }
                        payload.extend(wire_bytes(
                            batch.column(c_payload[fi]),
                            row,
                            f,
                            store,
                            &ns_header,
                        )?);
                    }
                    for (fi, f) in row_fields.iter().enumerate() {
                        if f.width > 0 {
                            continue;
                        }
                        let wb =
                            wire_bytes(batch.column(c_payload[fi]), row, f, store, &ns_header)?;
                        payload.push(fi as u8);
                        okm_core::put_len(&mut payload, wb.len());
                        payload.extend_from_slice(&wb);
                    }
                    heads.push(EdgeHead {
                        src,
                        dst,
                        predicate: predicates.value(row).to_string(),
                    });
                    ids.push(u64::from_be_bytes(id_bytes));
                    payloads.push(E::decode_payload(&payload));
                    props.push(variant_map(batch.column(c_variant), row)?);
                }
            }

            for ((head, (id, payload)), fields) in heads
                .iter()
                .zip(ids.iter().zip(payloads.iter()))
                .zip(props.iter())
            {
                // The id is the identity: `edge_at` creates it when free and
                // re-asserts it when live, and the reducer installs the payload
                // the file carries. One fragment, one commit.
                let payload = (*payload).clone();
                let mut fragment = Fragment::<E>::new([Decl::edge_at((*head).clone(), *id)]);
                fragment.reduce_edge(move |e: &mut E| *e = payload.clone());
                self.incorporate(fragment).map_err(|e| e.to_string())?;
                // Open attributes are the document's own row, so they take the
                // second commit (the two carriers are separate write paths).
                let mut batch = okm_core::engine::storage::Batch::default();
                self.replace_into(&mut batch, *id, fields)
                    .map_err(|e| e.to_string())?;
                self.store_mut()
                    .commit_batch(batch)
                    .map_err(|e| format!("edge {id}: {e}"))?;
            }
            Ok(n)
        }
    }

    /// The open-attributes column (a Parquet Variant holding one object) back to the map a
    /// write takes. A null cell is "no attributes", which is not an error; a cell
    /// that is not an object is one.
    fn variant_map(col: &dyn arrow::array::Array, row: usize) -> Result<Dict, String> {
        use arrow::array::{Array, BinaryArray, StructArray};

        // The canonical storage type of a variant: `struct<metadata: binary,
        // value: binary>`. A flat binary was okm's own invention and no reader decoded
        // it, which is why this reads the two halves and not a length header.
        let col = col
            .as_any()
            .downcast_ref::<StructArray>()
            .ok_or_else(|| format!("row {row}: the attribute column is not a variant struct"))?;
        if col.is_null(row) {
            return Ok(Dict::new());
        }
        let half = |name: &str| -> Result<&BinaryArray, String> {
            col.column_by_name(name)
                .and_then(|c| c.as_any().downcast_ref::<BinaryArray>())
                .ok_or_else(|| format!("row {row}: the variant has no `{name}` half"))
        };
        let (metadata, value) = (half("metadata")?, half("value")?);
        let (metadata, value) = (metadata.value(row), value.value(row));
        let variant = parquet_variant::Variant::new(metadata, value);
        let obj = variant
            .as_object()
            .ok_or_else(|| format!("row {row}: the attribute wire is not an object"))?;
        let mut out = Dict::new();
        for (name, value) in obj.iter() {
            out.insert(name.to_string(), to_dynamic(&value));
        }
        Ok(out)
    }
    fn to_dynamic(v: &parquet_variant::Variant<'_, '_>) -> okm_value::Variant {
        use okm_value::Variant;
        if v.as_null().is_some() {
            return Variant::Null;
        }
        if let Some(b) = v.as_boolean() {
            return Variant::Bool(b);
        }
        if let Some(s) = v.as_string() {
            return Variant::Str(s.to_string());
        }
        if let Some(b) = v.as_u8_slice() {
            return Variant::Bytes(b.to_vec());
        }
        if let Some(u) = v.as_u64() {
            return Variant::U64(u);
        }
        if let Some(i) = v.as_int64() {
            return Variant::I64(i);
        }
        if let Some(f) = v.as_f64() {
            return Variant::F64(f);
        }
        if let Some(list) = v.as_list() {
            return Variant::Array(list.iter().map(|it| to_dynamic(&it)).collect());
        }
        if let Some(obj) = v.as_object() {
            let mut map = Dict::new();
            for (name, val) in obj.iter() {
                map.insert(name.to_string(), to_dynamic(&val));
            }
            return Variant::Map(map);
        }
        Variant::Null
    }

    /// The restore half of [`export_reduces`]: read the derived table
    /// `(table, reduce, group, value)` and put each accumulator back, keeping the
    /// HIGHER of the two when the entry already exists.
    ///
    /// A restored table whose watermark is missing hands out ids it already used
    /// (ADR-0034 §5), so the watermark travels with the table — and a restore never
    /// LOWERS one, because a lower watermark re-issues ids that live rows still
    /// carry. `A::Acc: PartialOrd` is the price of that rule: the reduces that
    /// travel are watermarks (a running maximum), and a reduce whose accumulator
    /// has no order must not be pushed through this door.
    pub fn import_reduces<
        S: okm_core::engine::storage::VirtualStorage,
        A: okm_core::model::reduce::Reduce,
    >(
        store: &S,
        ns_prefix: &[u8],
        table: &str,
        path: &std::path::Path,
    ) -> parquet::errors::Result<usize>
    where
        A::Acc: PartialOrd,
    {
        use arrow::array::{Array, BinaryArray, StringArray, UInt16Array};
        use okm_core::model::reduce::ReduceCodec;

        let file = std::fs::File::open(path)?;
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
            .build()?;
        let mut prefix = ns_prefix.to_vec();
        okm_core::model::index::write_slot(&mut prefix, A::SLOT);

        let mut count = 0usize;
        for batch in reader {
            let batch: arrow::array::RecordBatch = batch?;
            if batch.num_columns() != 4 {
                return Err(parquet::errors::ParquetError::General(format!(
                    "reduce table has {} columns, expected (table, reduce, group, value)",
                    batch.num_columns()
                )));
            }
            let names = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| {
                    parquet::errors::ParquetError::General("column 0 is not a table name".into())
                })?;
            let slots = batch
                .column(1)
                .as_any()
                .downcast_ref::<UInt16Array>()
                .ok_or_else(|| {
                    parquet::errors::ParquetError::General("column 1 is not a reduce slot".into())
                })?;
            let groups = batch
                .column(2)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| {
                    parquet::errors::ParquetError::General("column 2 is not a group column".into())
                })?;
            let values = batch
                .column(3)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| {
                    parquet::errors::ParquetError::General("column 3 is not a value column".into())
                })?;
            for row in 0..batch.num_rows() {
                if names.value(row) != table {
                    return Err(parquet::errors::ParquetError::General(format!(
                        "row {row}: the file holds table `{}`, not `{table}`",
                        names.value(row)
                    )));
                }
                if slots.value(row) != A::SLOT {
                    return Err(parquet::errors::ParquetError::General(format!(
                        "row {row}: the file holds reduce slot {}, this table declares {}",
                        slots.value(row),
                        A::SLOT
                    )));
                }
                let mut key = prefix.clone();
                key.extend_from_slice(groups.value(row));
                let incoming = A::Acc::decode_acc(values.value(row));
                let existing = store
                    .get(&key)
                    .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
                let bytes = match existing {
                    Some(bytes) => {
                        let current = A::Acc::decode_acc(&bytes);
                        if current >= incoming {
                            continue;
                        }
                        incoming.encode_acc()
                    }
                    None => incoming.encode_acc(),
                };
                store
                    .put(key, bytes)
                    .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
                count += 1;
            }
        }
        Ok(count)
    }
}
