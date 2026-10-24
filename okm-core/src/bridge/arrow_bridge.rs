//! Arrow RecordBatch bridge (ADR-0007 Phase 1 — eager export).
//!
//! Streams a collection's documents as Arrow `RecordBatch`es: the key fields and the
//! TLV payload fields become columns, with the schema generated from the
//! same `FieldDesc` tables the derive macros emit ("code as DDL" — one
//! struct is the single source for key encoding, payload encoding, index
//! slots, snapshot columns, and Arrow schema).
//!
//! This module reads bytes, not structs: key fields are big-endian slices
//! of the key encoding, payload fields are TLV-framed slices of the document
//! payload. Column builders copy those slices straight in — no intermediate
//! struct materialization on the export path.
//!
//! Read-only by contract (ADR-0007 Boundaries): nothing here writes back.

use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Field as ArrowField, Schema};

use crate::engine::storage::VirtualStorage;
use crate::model::field::{FieldDesc, FieldType};
use crate::model::index::Document;
use crate::model::key::KeyEncode;
use crate::model::obj_dynamic::DynamicValue;
use crate::model::document::Collection;
use crate::model::graph::{EdgeKey, Graph, GraphEncode};

/// Arrow column type for a declared field kind.
fn arrow_type(ty: FieldType) -> DataType {
    match ty {
        FieldType::U8 => DataType::UInt8,
        FieldType::U16 => DataType::UInt16,
        FieldType::U32 => DataType::UInt32,
        FieldType::U64 => DataType::UInt64,
        FieldType::FixedBytes => DataType::Binary,
        FieldType::Str => DataType::Utf8,
        FieldType::Bytes => DataType::Binary,
        // Logical types: VarInt decodes to its integer, Quant dequantizes
        // to f64, Offset re-adds the base.
        FieldType::VarInt => DataType::UInt64,
        FieldType::Quant(_) => DataType::Float64,
        // A categorical value exports its LABEL (ADR-0032): the id is the
        // store's internal numbering, not something a warehouse can name. The
        // column is nullable because a row may name an id the vocabulary does
        // not know — the export records that hole instead of inventing a label.
        FieldType::Vocab(_) => DataType::Utf8,
        FieldType::Offset(_) => DataType::Int64,
        // Variable-length frame → opaque binary column (like Bytes); the
        // element type lives in the schema, Arrow stays byte-opaque.
        FieldType::Vector { .. } => DataType::Binary,
    }
}

/// Decode a prefix-monotonic varint (P1 wire — the same codec as
/// `VarIntEnc`, shared shape) for `VarInt` columns. `raw` is the frame
/// value region only.
fn varint_decode(b: &[u8]) -> (u64, usize) {
    let w = crate::model::wrappers::wire::prefix_width(
        *b.first().expect("VarInt: truncated frame"),
    );
    assert!(b.len() >= w, "VarInt: truncated frame");
    if w == 9 {
        return (u64::from_be_bytes(b[1..9].try_into().unwrap()), 9);
    }
    let hi = (b[0] & (0xFF >> w)) as u64;
    let mut low: u64 = 0;
    for byte in &b[1..w] {
        low = (low << 8) | *byte as u64;
    }
    (hi << ((w - 1) * 8) | low, w)
}

/// Wire slice → logical value bytes (little-endian) for the transformed
/// payload kinds. Str/VarInt are variable-length and handled frame-wise;
/// the rest are fixed-offset.
fn logical_value(raw: &[u8], ty: FieldType) -> Vec<u8> {
    match ty {
        FieldType::VarInt => varint_decode(raw).0.to_le_bytes().to_vec(),
        FieldType::Quant(p) => {
            let w = i64::from_be_bytes(raw.try_into().expect("quant width"));
            (w as f64 / 10f64.powi(p as i32)).to_le_bytes().to_vec()
        }
        FieldType::Vocab(_) => raw.to_vec(),
        FieldType::Offset(base) => {
            let off = u32::from_be_bytes(raw.try_into().expect("offset width")) as i64;
            (base + off).to_le_bytes().to_vec()
        }
        other => swap_be(raw, 0, raw.len(), other),
    }
}

/// Columnar projection of a collection: key fields then payload fields, in
/// declaration order within each half. Two sources, one schema.
struct Projection<K: KeyEncode, R: Document<Key = K>> {
    schema: Schema,
    key_fields: &'static [FieldDesc],
    row_fields: &'static [FieldDesc],
    _marker: std::marker::PhantomData<(K, R)>,
}

impl<K: KeyEncode, R: Document<Key = K>> Projection<K, R> {
    fn new() -> Self {
        let key_fields = <K as KeyEncode>::FIELDS;
        let row_fields = <R as Document>::FIELDS;
        let mut fields = Vec::with_capacity(key_fields.len() + row_fields.len());
        for f in key_fields {
            fields.push(ArrowField::new(f.name, arrow_type(f.ty), false));
        }
        for f in row_fields {
            // Every payload column can hold a NULL, for two reasons: a document
            // with no row (ADR-0035's segment-only document — the whole payload
            // half is NULL, nothing is invented) and a `Vocab` id the field's
            // vocabulary does not name (see `vocab_labels`). Key columns stay
            // non-nullable: every document has an identity.
            fields.push(ArrowField::new(f.name, arrow_type(f.ty), true));
        }
        Self {
            schema: Schema::new(fields),
            key_fields,
            row_fields,
            _marker: std::marker::PhantomData,
        }
    }

    fn total_width(&self) -> usize {
        self.key_fields.iter().map(|f| f.width).sum::<usize>()
            + self.row_fields.iter().map(|f| f.width).sum::<usize>()
    }
}

/// Per-row-field `id bytes → label` maps for the categorical fields (ADR-0032),
/// each from ONE prefix scan of that field's id face. Empty for a table with no
/// `Vocab` field.
///
/// The scan is bounded by the VOCABULARY, not by the row count, and export is a
/// read path, so it is legal here (`BatchStore` refuses ordered reads, but a
/// snapshot never runs against a pending batch). Rows then resolve by an
/// in-memory lookup on the id bytes — no per-row probe, and no width matching:
/// the map key IS the payload slice.
fn vocab_labels<S: VirtualStorage + ?Sized>(
    store: &S,
    header: &[u8],
    row_fields: &'static [FieldDesc],
) -> crate::error::Result<std::collections::HashMap<usize, std::collections::HashMap<Vec<u8>, String>>> {
    let mut out = std::collections::HashMap::new();
    for (fi, f) in row_fields.iter().enumerate() {
        let FieldType::Vocab(id_slot) = f.ty else {
            continue;
        };
        let mut prefix = header.to_vec();
        crate::model::index::write_slot(&mut prefix, id_slot);
        let mut by_id = std::collections::HashMap::new();
        for (suffix, label) in crate::engine::storage::scan_suffix_kv(store, &prefix)? {
            // A real entry of the id face is exactly the id width; the
            // bare-prefix singleton (the allocation cursor) is zero bytes and
            // falls out here by length.
            if suffix.len() != f.width {
                continue;
            }
            if let Ok(name) = String::from_utf8(label) {
                by_id.insert(suffix, name);
            }
        }
        out.insert(fi, by_id);
    }
    Ok(out)
}

/// The label maps for one collection: its ns header (`[part][ns]`, the same one
/// the intern hook writes through) plus the row fields of its projection.
/// Tables with no `Vocab` field pay one constant check and no scan.
fn labels_for<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>>(
    collection: &Collection<S, K, R>,
    proj: &Projection<K, R>,
) -> crate::error::Result<std::collections::HashMap<usize, std::collections::HashMap<Vec<u8>, String>>> {
    if !R::HAS_VOCAB {
        return Ok(std::collections::HashMap::new());
    }
    let mut header = R::PARTITION_PREFIX.to_vec();
    header.extend_from_slice(R::NS_PREFIX);
    vocab_labels::<S>(collection.store(), &header, proj.row_fields)
}

/// Buffer adapter: arrow wants `Buffer` + length; the bytes come from a
/// caller-owned `Vec`. Each column is a contiguous BE run of the source
/// bytes, so per-column we produce (offset, len) views of the document buffer.
///
/// One `RecordBatch` per call: documents are appended column-wise into
/// `Vec<Vec<u8>>` scratch (column-major), then each column becomes a single
/// `Buffer` of concatenated fixed-width values — arrow primitives are
/// little-endian, so a byte-swapping copy is required per value.
fn build_batch<K: KeyEncode, R: Document<Key = K>>(
    proj: &Projection<K, R>,
    documents: &[(Vec<u8>, Option<Vec<u8>>)],
    labels: &std::collections::HashMap<usize, std::collections::HashMap<Vec<u8>, String>>,
) -> RecordBatch {
    let n = documents.len();
    let mut columns: Vec<Vec<Option<Vec<u8>>>> = Vec::with_capacity(proj.schema.fields().len());

    // Precompute the declaration-order byte offset of every key field
    // (computed once, applied per document).
    let key_offsets: Vec<usize> = {
        let mut offs = Vec::with_capacity(proj.key_fields.len());
        let mut acc = 0usize;
        for f in proj.key_fields {
            offs.push(acc);
            acc += f.width;
        }
        offs
    };

    // Key half: the scan suffix IS the key payload (encoding = declaration-
    // order BE fields), so per-field runs are contiguous offsets into it.
    // One value per document per column (column-major list of per-document values).
    for (fi, f) in proj.key_fields.iter().enumerate() {
        let mut col: Vec<Option<Vec<u8>>> = Vec::with_capacity(n);
        for (kenc, _) in documents {
            let off = key_offsets[fi];
            col.push(Some(swap_be(kenc, off, f.width, f.ty)));
        }
        columns.push(col);
    }
    // Payload half: two-segment layout behind the 3-byte header — hot
    // segment is fixed-width at static offsets; cold segment is TLV walked
    // frame-by-frame (each preceding variable-length frame shifts it).
    let hot_width = <R as Document>::HOT_WIDTH;
    let header = 3usize; // version u8 + hot_len u16
    let cold_fields: Vec<(usize, &FieldDesc)> = proj
        .row_fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.width == 0)
        .collect();
    for (fi, f) in proj.row_fields.iter().enumerate() {
        let mut col: Vec<Option<Vec<u8>>> = Vec::with_capacity(n);
        for (_, v) in documents {
            // No row at all: the whole payload half is NULL for this document.
            // Nothing is inverted into a default — the row simply is not there.
            let Some(v) = v.as_deref() else {
                col.push(None);
                continue;
            };
            if f.width > 0 {
                // Hot: header + sum of prior hot field widths.
                let off = header
                    + proj.row_fields[..fi]
                        .iter()
                        .filter(|g| g.width > 0)
                        .map(|g| g.width)
                        .sum::<usize>();
                col.push(Some(logical_value(&v[off..off + f.width], f.ty)));
            } else {
                let val = tlv_value(&v[header + hot_width..], &cold_fields, fi);
                if f.ty == FieldType::Str {
                    col.push(Some(val.to_vec()));
                } else {
                    col.push(Some(logical_value(val, f.ty)));
                }
            }
        }
        columns.push(col);
    }

    // Field descs in column order (key half, then payload half): assembly
    // dispatches on the DECLARED type, not the Arrow type — a `Str` column and
    // a `Vocab` column are both Utf8, and only the latter holds ids to resolve.
    let descs: Vec<&FieldDesc> = proj.key_fields.iter().chain(proj.row_fields.iter()).collect();
    let nkey = proj.key_fields.len();
    // A `None` cell is an Arrow NULL: no row (segment-only document) or a Vocab
    // id the vocabulary does not name. Key cells are never NULL.
    let arrays: Vec<Arc<dyn arrow::array::Array>> = columns
        .iter()
        .enumerate()
        .map(|(ci, col)| -> Arc<dyn arrow::array::Array> {
            let field = &proj.schema.fields()[ci];
            if let FieldType::Vocab(_) = descs[ci].ty {
                // id bytes → label; a hole stays NULL (never an invented name).
                // Key columns have no map (a `Vocab` key is not a supported
                // declaration), so they answer NULL rather than panicking.
                let empty = std::collections::HashMap::new();
                let by_id = ci
                    .checked_sub(nkey)
                    .and_then(|fi| labels.get(&fi))
                    .unwrap_or(&empty);
                let vals: Vec<Option<&str>> = col
                    .iter()
                    .map(|raw| raw.as_ref().and_then(|r| by_id.get(r).map(String::as_str)))
                    .collect();
                return Arc::new(arrow::array::StringArray::from_iter(vals));
            }
            if matches!(field.data_type(), DataType::Binary) {
                return Arc::new(arrow::array::BinaryArray::from_iter(
                    col.iter().map(|c| c.as_deref()),
                ));
            }
            if matches!(field.data_type(), DataType::Utf8) {
                return Arc::new(arrow::array::StringArray::from_iter(col.iter().map(|c| {
                    c.as_deref()
                        .map(|b| std::str::from_utf8(b).expect("Utf8 column holds UTF-8"))
                })));
            }
            match field.data_type() {
            DataType::UInt8 => {
                let vals: Vec<Option<u8>> = col.iter().map(|c| c.as_ref().map(|b| b[0])).collect();
                Arc::new(arrow::array::UInt8Array::from_iter(vals))
            }
            DataType::UInt16 => {
                let vals: Vec<Option<u16>> = col
                    .iter()
                    .map(|c| c.as_ref().map(|b| u16::from_le_bytes(b.as_chunks::<2>().0[0])))
                    .collect();
                Arc::new(arrow::array::UInt16Array::from_iter(vals))
            }
            DataType::UInt32 => {
                let vals: Vec<Option<u32>> = col
                    .iter()
                    .map(|c| c.as_ref().map(|b| u32::from_le_bytes(b.as_chunks::<4>().0[0])))
                    .collect();
                Arc::new(arrow::array::UInt32Array::from_iter(vals))
            }
            DataType::UInt64 => {
                let vals: Vec<Option<u64>> = col
                    .iter()
                    .map(|c| c.as_ref().map(|b| u64::from_le_bytes(b.as_chunks::<8>().0[0])))
                    .collect();
                Arc::new(arrow::array::UInt64Array::from_iter(vals))
            }
            DataType::Float64 => {
                let vals: Vec<Option<f64>> = col
                    .iter()
                    .map(|c| c.as_ref().map(|b| f64::from_le_bytes(b.as_chunks::<8>().0[0])))
                    .collect();
                Arc::new(arrow::array::Float64Array::from_iter(vals))
            }
            DataType::Int64 => {
                let vals: Vec<Option<i64>> = col
                    .iter()
                    .map(|c| c.as_ref().map(|b| i64::from_le_bytes(b.as_chunks::<8>().0[0])))
                    .collect();
                Arc::new(arrow::array::Int64Array::from_iter(vals))
            }
            other => unreachable!("no bridge mapping for {other:?}"),
            }
        })
        .collect();

    RecordBatch::try_new(Arc::new(proj.schema.clone()), arrays).expect("batch construction")
}

/// DynamicValue -> parquet Variant via the builder API (nested Obj/Array
/// recurse; scalars map 1:1). UInt widens to i64 — Variant integers are
/// signed, and OKM UInt fields are u64: values above i64::MAX wrap. This
/// is a documented export limitation, not a storage change.
#[cfg(feature = "parquet")]
fn push_variant<B: parquet_variant::VariantBuilderExt>(b: &mut B, v: &DynamicValue) {
    match v {
        DynamicValue::Null => b.append_null(),
        DynamicValue::Bool(x) => b.append_value(*x),
        DynamicValue::UInt(x) => b.append_value(*x as i64),
        DynamicValue::Int(x) => b.append_value(*x),
        DynamicValue::F64(x) => b.append_value(*x),
        DynamicValue::Str(sv) => b.append_value(sv.as_str()),
        DynamicValue::Bytes(bv) => b.append_value(bv.as_slice()),
        DynamicValue::Array(items) => {
            let mut lb = b.new_list();
            for it in items { push_variant(&mut lb, it); }
            lb.finish();
        }
        DynamicValue::Obj(map) => {
            // ObjectBuilder does not impl VariantBuilderExt — fields go
            // through keyed sub-builders / field builders.
            let mut ob = b.new_object();
            push_variant_obj(&mut ob, map);
            ob.finish();
        }
    }
}

#[cfg(feature = "parquet")]
fn push_variant_obj<S: parquet_variant::BuilderSpecificState>(
    ob: &mut parquet_variant::ObjectBuilder<'_, S>,
    map: &std::collections::BTreeMap<String, DynamicValue>,
) {
    for (k, val) in map {
        match val {
            DynamicValue::Obj(sub_map) => {
                let mut sub = ob.new_object(k);
                push_variant_obj(&mut sub, sub_map);
                sub.finish();
            }
            DynamicValue::Array(items) => {
                let mut sub = ob.new_list(k);
                for it in items { push_variant(&mut sub, it); }
                sub.finish();
            }
            other => {
                let mut fb = parquet_variant::ObjectFieldBuilder::new(k, ob);
                push_variant(&mut fb, other);
            }
        }
    }
}

/// A Variant-typed Arrow extension field ("parquet.variant" extension
/// metadata), the analytics-layer twin of the dynamic segment.
#[cfg(feature = "parquet")]
fn variant_field(name: &str, nullable: bool) -> arrow::datatypes::Field {
    use arrow::datatypes::Field as ArrowField;
    let mut f = ArrowField::new(name, DataType::Binary, nullable);
    // Variant's Arrow representation: binary metadata+value; the extension
    // type marks it. Full StructArray representation is a later refinement;
    // Binary + extension metadata is what the spec calls the "shredding
    // absent" fallback and every Variant-aware engine accepts it.
    f.set_metadata(std::collections::HashMap::from([(
        "ARROW:extension:name".to_string(),
        "parquet.variant".to_string(),
    )]));
    f
}

/// Value slice of cold field `fi` (declaration index) inside the cold TLV
/// region, walked frame-by-frame. `cold_fields` lists the declaration
/// indices of the width-0 fields in order; a frame's tag IS that
/// declaration index. Returns the value region only (tag + len skipped).
fn tlv_value<'a>(
    cold: &'a [u8],
    cold_fields: &[(usize, &FieldDesc)],
    fi: usize,
) -> &'a [u8] {
    let mut off = 0usize;
    for (decl, f) in cold_fields {
        // P2 wire: frame = [tag u8][len varint][value] — take_len reads
        // the prefix-monotonic length and reports its byte width.
        let (len, len_n) = crate::take_len(&cold[off + 1..])
            .expect("cold frame: truncated length prefix");
        let val = off + 1 + len_n;
        if *decl == fi {
            // A cold frame carries its own length; a FIXED-width field never
            // reaches this path. `width == 0` IS the repo's definition of
            // "variable length", so it covers Str, VarInt, Bytes and Vector
            // alike — enumerating a subset of them was how `Vector` slipped
            // through and tripped this assertion on every exported ring.
            debug_assert!(f.width == 0 || len == f.width);
            return &cold[val..val + len];
        }
        off = val + len;
    }
    unreachable!("field index out of cold TLV frame range: {fi}");
}

/// Copy `width` bytes at `off` of a big-endian region into the value bytes
/// arrow's little-endian arrays expect. Multi-byte integers are byte-
/// swapped; single bytes and binaries are copied verbatim.
///
/// Variable-length (`Str`) columns can't use this fixed-offset copy — they
/// are handled frame-by-frame in `build_batch` below.
fn swap_be(src: &[u8], off: usize, width: usize, ty: FieldType) -> Vec<u8> {
    debug_assert_ne!(ty, FieldType::Str);
    let raw = &src[off..off + width];
    match ty {
        FieldType::U8 | FieldType::FixedBytes => raw.to_vec(),
        FieldType::U16 => raw.iter().rev().copied().collect(),
        FieldType::U32 => raw.iter().rev().copied().collect(),
        FieldType::U64 => raw.iter().rev().copied().collect(),
        FieldType::Vector { .. } => raw.to_vec(), // already LE by contract
        FieldType::Str | FieldType::Bytes => unreachable!("Str/Bytes columns bypass swap_be"),
        FieldType::VarInt | FieldType::Quant(_) | FieldType::Vocab(_) | FieldType::Offset(_) => {
            unreachable!("transformed kinds bypass swap_be (see logical_value)")
        }
    }
}

impl<S: VirtualStorage, K: KeyEncode, R: Document<Key = K>> Collection<S, K, R> {
    /// Export all documents as one Arrow `RecordBatch` (ADR-0007 Phase 1).
    ///
    /// Columns: key fields (declaration order) then payload fields. Key and
    /// ordinary payload columns are non-nullable; a `Vocab` column is `Utf8`
    /// (labels) and NULLABLE — null means the row's id has no label in the
    /// field's vocabulary, which the export records rather than invents.
    ///
    /// Reads through the engine's scan surface — engine-independent, same
    /// batch shape for fjall / slatedb-backed tables. Categorical fields cost
    /// one prefix scan of their id face, not a probe per row.
    pub fn to_record_batch(&self) -> crate::error::Result<RecordBatch> {
        let proj = Projection::<K, R>::new();
        let documents = self.scan_documents_raw()?;
        let labels = labels_for(self, &proj)?;
        Ok(build_batch(&proj, &documents, &labels))
    }

    /// Export with a `variant` column appended: each row's dynamic segment
    /// (slot 1) encoded as one Parquet Variant value (nested Obj/Array
    /// recurse; names resolved through the field-name vocabulary). Rows
    /// without a dynamic segment get `null`. The declared fields stay
    /// typed columns — Variant is the analytics-layer twin of the
    /// dynamic path, not a replacement for it (ADR-0007).
    #[cfg(feature = "parquet")]
    pub fn to_record_batch_with_variant(&mut self) -> crate::error::Result<RecordBatch> {
        use arrow::array::BinaryArray;
        use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
        use arrow::datatypes::{Field as ArrowField, Schema as ArrowSchema};

        let proj = Projection::<K, R>::new();
        let documents = self.scan_documents_raw()?;

        // Base (typed) columns from the same projection.
        let labels = labels_for(self, &proj)?;
        let base = build_batch(&proj, &documents, &labels);

        // One Variant per document from its dynamic segment: metadata and
        // value buffers CONCATENATED per row (the Variant spec's wire form
        // is [metadata][value] — a single binary per row).
        let mut per_row: Vec<Option<Vec<u8>>> = Vec::with_capacity(documents.len());
        for (kenc, _) in &documents {
            let key = K::decode(kenc);
            match self.slot1_map(&key)? {
                Some(fields) => {
                    let mut b = parquet_variant::VariantBuilder::new();
                    let map: std::collections::BTreeMap<String, DynamicValue> =
                        fields.into_iter().collect();
                    push_variant(&mut b, &DynamicValue::Obj(map));
                    let (md, val) = b.finish();
                    // Wire per row: [md_len u32 BE][metadata][value] — the
                    // metadata vocab is self-contained per variant value, so
                    // the reader splits deterministically.
                    let mut wire = (md.len() as u32).to_be_bytes().to_vec();
                    wire.extend_from_slice(&md);
                    wire.extend_from_slice(&val);
                    per_row.push(Some(wire));
                }
                None => per_row.push(None),
            }
        }

        let mut offsets: Vec<i32> = Vec::with_capacity(documents.len() + 1);
        offsets.push(0);
        let mut data = Vec::new();
        let mut nulls = Vec::with_capacity(documents.len());
        for wire in &per_row {
            match wire {
                Some(bytes) => {
                    data.extend_from_slice(bytes);
                    offsets.push(data.len() as i32);
                    nulls.push(true);
                }
                None => {
                    offsets.push(data.len() as i32);
                    nulls.push(false);
                }
            }
        }
        let variant_col = Arc::new(BinaryArray::new(
            OffsetBuffer::new(ScalarBuffer::from(offsets)),
            Buffer::from(data),
            Some(arrow::buffer::NullBuffer::from(nulls)),
        ));

        // Schema: base fields + variant field (extension-typed).
        let mut fields: Vec<ArrowField> = base
            .schema()
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        fields.push(variant_field("variant", true));
        let schema = Arc::new(ArrowSchema::new(fields));

        let mut arrays: Vec<Arc<dyn arrow::array::Array>> =
            base.columns().to_vec();
        arrays.push(variant_col);

        // Assembly invariants (column count vs schema) are this function's
        // own shape, not a store condition — the fallible part above is the read.
        Ok(RecordBatch::try_new(schema, arrays).expect("batch with variant column"))
    }

    /// Exported column names (key fields then payload fields) with their
    /// Arrow types — the schema as declared, handy for callers assembling
    /// downstream tables.
    pub fn export_columns(&self) -> Vec<(&'static str, DataType)> {
        let proj = Projection::<K, R>::new();
        proj.key_fields
            .iter()
            .chain(proj.row_fields)
            .map(|f| (f.name, arrow_type(f.ty)))
            .collect()
    }

    /// Approximate per-batch memory: one document's full field width.
    pub fn row_width(&self) -> usize {
        Projection::<K, R>::new().total_width()
    }
}

// ---- the graph's edge set as ONE table (ADR-0034) -------------------------

impl<S: VirtualStorage, E: GraphEncode> Graph<S, E> {
    /// The graph's edges as one `RecordBatch`, in the shape a table has:
    /// `[src_table, src_pkey, dst_table, dst_pkey, predicate]`, then the edge
    /// declaration's own fields, then the edge's OPEN ATTRIBUTES as a `variant`
    /// column (the 0x01 segment, if the store has one for that edge).
    ///
    /// The endpoints are (table, primary key) pairs — what a node reference IS —
    /// with the pkey as RAW BYTES: one edge table spans node collections, so it
    /// cannot decode another table's key into that table's fields without that
    /// table's declaration. `name_of` maps an ns to its table name; an ns it does
    /// not know yields a null cell, never an invented name.
    pub fn to_record_batch_table<F>(
        &mut self,
        name_of: F,
    ) -> crate::error::Result<RecordBatch>
    where
        F: Fn(u16) -> Option<String>,
    {
        use arrow::array::{Array, BinaryArray};
        use arrow::buffer::{Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
        use arrow::datatypes::{Field as ArrowField, Schema as ArrowSchema};

        let ids = self.edge_ids()?;
        // Edge rows always have a body: `None` (no row) does not arise here.
        let mut documents: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::with_capacity(ids.len());
        // (src name, src pkey, dst name, dst pkey, predicate) per row
        let mut rows: Vec<(Option<String>, Vec<u8>, Option<String>, Vec<u8>, String)> =
            Vec::with_capacity(ids.len());
        let mut variants: Vec<Option<Vec<u8>>> = Vec::with_capacity(ids.len());

        for id in ids {
            let Some(body) = self.get_edge(id)? else { continue };
            let predicate = self
                .predicate_vocab_name(body.predicate_vocab_id)?
                .unwrap_or_default();
            rows.push((
                name_of(body.src.ns_id()),
                body.src.pkey.clone(),
                name_of(body.dst.ns_id()),
                body.dst.pkey.clone(),
                predicate,
            ));
            variants.push(self.attrs(id)?.map(|fields| {
                let mut b = parquet_variant::VariantBuilder::new();
                let map: std::collections::BTreeMap<String, DynamicValue> =
                    fields.into_iter().collect();
                push_variant(&mut b, &DynamicValue::Obj(map));
                let (md, val) = b.finish();
                let mut wire = (md.len() as u32).to_be_bytes().to_vec();
                wire.extend_from_slice(&md);
                wire.extend_from_slice(&val);
                wire
            }));
            documents.push((id.to_be_bytes().to_vec(), Some(body.payload)));
        }

        // The declaration's own columns: the edge key (the bare edge id) then the
        // payload fields, exactly the document projection.
        let proj = Projection::<EdgeKey, E>::new();
        let mut header = E::PARTITION_PREFIX.to_vec();
        header.extend_from_slice(E::NS_PREFIX);
        let labels = if E::HAS_VOCAB {
            vocab_labels::<S>(self.store(), &header, proj.row_fields)?
        } else {
            std::collections::HashMap::new()
        };
        let base = build_batch(&proj, &documents, &labels);

        // The table columns, in front.
        let string_col = |vals: &[Option<String>]| -> Arc<dyn Array> {
            let mut buf = arrow::array::StringBuilder::new();
            for v in vals {
                match v {
                    Some(s) => buf.append_value(s),
                    None => buf.append_null(),
                }
            }
            Arc::new(buf.finish())
        };
        let binary_col = |vals: &[Vec<u8>]| -> Arc<dyn Array> {
            let mut offsets: Vec<i32> = Vec::with_capacity(vals.len() + 1);
            offsets.push(0);
            let mut data = Vec::new();
            for v in vals {
                data.extend_from_slice(v);
                offsets.push(data.len() as i32);
            }
            Arc::new(BinaryArray::new(
                OffsetBuffer::new(ScalarBuffer::from(offsets)),
                Buffer::from(data),
                None,
            ))
        };

        let src_tables: Vec<Option<String>> = rows.iter().map(|r| r.0.clone()).collect();
        let src_pkeys: Vec<Vec<u8>> = rows.iter().map(|r| r.1.clone()).collect();
        let dst_tables: Vec<Option<String>> = rows.iter().map(|r| r.2.clone()).collect();
        let dst_pkeys: Vec<Vec<u8>> = rows.iter().map(|r| r.3.clone()).collect();
        let predicates: Vec<Option<String>> = rows.iter().map(|r| Some(r.4.clone())).collect();

        let mut fields: Vec<ArrowField> = vec![
            ArrowField::new("src_table", DataType::Utf8, true),
            ArrowField::new("src_pkey", DataType::Binary, false),
            ArrowField::new("dst_table", DataType::Utf8, true),
            ArrowField::new("dst_pkey", DataType::Binary, false),
            ArrowField::new("predicate", DataType::Utf8, false),
        ];
        for f in base.schema().fields() {
            fields.push(f.as_ref().clone());
        }
        fields.push(variant_field("variant", true));
        let schema = Arc::new(ArrowSchema::new(fields));

        let mut arrays: Vec<Arc<dyn Array>> = vec![
            string_col(&src_tables),
            binary_col(&src_pkeys),
            string_col(&dst_tables),
            binary_col(&dst_pkeys),
            string_col(&predicates),
        ];
        arrays.extend(base.columns().iter().cloned());

        // The variant column, same wire rule as the document path.
        let mut offsets: Vec<i32> = Vec::with_capacity(variants.len() + 1);
        offsets.push(0);
        let mut data = Vec::new();
        let mut nulls = Vec::with_capacity(variants.len());
        for wire in &variants {
            match wire {
                Some(bytes) => {
                    data.extend_from_slice(bytes);
                    offsets.push(data.len() as i32);
                    nulls.push(true);
                }
                None => {
                    offsets.push(data.len() as i32);
                    nulls.push(false);
                }
            }
        }
        arrays.push(Arc::new(BinaryArray::new(
            OffsetBuffer::new(ScalarBuffer::from(offsets)),
            Buffer::from(data),
            Some(NullBuffer::from(nulls)),
        )));

        // Same split as the document export: the read above is fallible, the
        // column-vs-schema shape is this function's own invariant.
        Ok(RecordBatch::try_new(schema, arrays).expect("edge table batch"))
    }
}
