//! Arrow RecordBatch bridge (ADR-0007 Phase 1 — eager export).
//!
//! Streams a table's rows as Arrow `RecordBatch`es: the key fields and the
//! TLV payload fields become columns, with the schema generated from the
//! same `FieldDesc` tables the derive macros emit ("code as DDL" — one
//! struct is the single source for key encoding, payload encoding, index
//! slots, snapshot columns, and Arrow schema).
//!
//! This module reads bytes, not structs: key fields are big-endian slices
//! of the key encoding, payload fields are TLV-framed slices of the row
//! payload. Column builders copy those slices straight in — no intermediate
//! struct materialization on the export path.
//!
//! Read-only by contract (ADR-0007 Boundaries): nothing here writes back.

use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::buffer::Buffer;
use arrow::datatypes::{DataType, Field as ArrowField, Schema};

use crate::engine::KvEngine;
use crate::field::{FieldDesc, FieldType};
use crate::index::Row;
use crate::key::KeyEncode;
use crate::table::Table;

/// Arrow column type for a declared field kind.
fn arrow_type(ty: FieldType) -> DataType {
    match ty {
        FieldType::U8 => DataType::UInt8,
        FieldType::U16 => DataType::UInt16,
        FieldType::U32 => DataType::UInt32,
        FieldType::U64 => DataType::UInt64,
        FieldType::FixedBytes => DataType::Binary,
        FieldType::Str => DataType::Utf8,
        // Logical types: VarInt decodes to its integer, Quant dequantizes
        // to f64, Offset re-adds the base.
        FieldType::VarInt => DataType::UInt64,
        FieldType::Quant(_) => DataType::Float64,
        FieldType::Enum => DataType::UInt8,
        FieldType::Offset(_) => DataType::Int64,
    }
}

/// LEB128 decode for `VarInt` columns (mirror of `VarIntEnc::varint_decode`).
fn varint_decode(b: &[u8]) -> (u64, usize) {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    let mut i = 0usize;
    loop {
        let byte = *b.get(i).expect("VarInt: truncated frame");
        v |= ((byte & 0x7F) as u64) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        assert!(i < 10, "VarInt: continuation byte past max width");
    }
    (v, i)
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
        FieldType::Enum => raw.to_vec(),
        FieldType::Offset(base) => {
            let off = u32::from_be_bytes(raw.try_into().expect("offset width")) as i64;
            (base + off).to_le_bytes().to_vec()
        }
        other => swap_be(raw, 0, raw.len(), other),
    }
}

/// Columnar projection of a table: key fields then payload fields, in
/// declaration order within each half. Two sources, one schema.
struct Projection<K: KeyEncode, R: Row<Key = K>> {
    schema: Schema,
    key_fields: &'static [FieldDesc],
    row_fields: &'static [FieldDesc],
    _marker: std::marker::PhantomData<(K, R)>,
}

impl<K: KeyEncode, R: Row<Key = K>> Projection<K, R> {
    fn new() -> Self {
        let key_fields = <K as KeyEncode>::FIELDS;
        let row_fields = <R as Row>::FIELDS;
        let mut fields = Vec::with_capacity(key_fields.len() + row_fields.len());
        for f in key_fields {
            fields.push(ArrowField::new(f.name, arrow_type(f.ty), false));
        }
        for f in row_fields {
            fields.push(ArrowField::new(f.name, arrow_type(f.ty), false));
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

/// Buffer adapter: arrow wants `Buffer` + length; the bytes come from a
/// caller-owned `Vec`. Each column is a contiguous BE run of the source
/// bytes, so per-column we produce (offset, len) views of the row buffer.
///
/// One `RecordBatch` per call: rows are appended column-wise into
/// `Vec<Vec<u8>>` scratch (column-major), then each column becomes a single
/// `Buffer` of concatenated fixed-width values — arrow primitives are
/// little-endian, so a byte-swapping copy is required per value.
fn build_batch<K: KeyEncode, R: Row<Key = K>>(
    proj: &Projection<K, R>,
    rows: &[(Vec<u8>, Vec<u8>)],
) -> RecordBatch {
    let n = rows.len();
    let mut columns: Vec<Vec<Vec<u8>>> = Vec::with_capacity(proj.schema.fields().len());

    // Precompute the declaration-order byte offset of every key field
    // (computed once, applied per row).
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
    // One value per row per column (column-major list of per-row values).
    for (fi, f) in proj.key_fields.iter().enumerate() {
        let mut col = Vec::with_capacity(n);
        for (kenc, _) in rows {
            let off = key_offsets[fi];
            col.push(swap_be(kenc, off, f.width, f.ty));
        }
        columns.push(col);
    }
    // Payload half: two-segment layout behind the 3-byte header — hot
    // segment is fixed-width at static offsets; cold segment is TLV walked
    // frame-by-frame (each preceding variable-length frame shifts it).
    let hot_width = <R as Row>::HOT_WIDTH;
    let header = 3usize; // version u8 + hot_len u16
    let cold_fields: Vec<(usize, &FieldDesc)> = proj
        .row_fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.width == 0)
        .collect();
    for (fi, f) in proj.row_fields.iter().enumerate() {
        let mut col: Vec<Vec<u8>> = Vec::with_capacity(n);
        for (_, v) in rows {
            if f.width > 0 {
                // Hot: header + sum of prior hot field widths.
                let off = header
                    + proj.row_fields[..fi]
                        .iter()
                        .filter(|g| g.width > 0)
                        .map(|g| g.width)
                        .sum::<usize>();
                col.push(logical_value(&v[off..off + f.width], f.ty));
            } else {
                let val = tlv_value(&v[header + hot_width..], &cold_fields, fi);
                if f.ty == FieldType::Str {
                    col.push(val.to_vec());
                } else {
                    col.push(logical_value(val, f.ty));
                }
            }
        }
        columns.push(col);
    }

    let arrays: Vec<Arc<dyn arrow::array::Array>> = columns
        .iter()
        .zip(proj.schema.fields().iter())
        .map(|(col, field)| -> Arc<dyn arrow::array::Array> {
            match field.data_type() {
            DataType::UInt8 => {
                let vals: Vec<u8> = col.iter().map(|c| c[0]).collect();
                Arc::new(arrow::array::UInt8Array::from(vals))
            }
            DataType::UInt16 => {
                let vals: Vec<u16> = col.iter().map(|c| u16::from_le_bytes(c.as_chunks::<2>().0[0])).collect();
                Arc::new(arrow::array::UInt16Array::from(vals))
            }
            DataType::UInt32 => {
                let vals: Vec<u32> = col.iter().map(|c| u32::from_le_bytes(c.as_chunks::<4>().0[0])).collect();
                Arc::new(arrow::array::UInt32Array::from(vals))
            }
            DataType::UInt64 => {
                let vals: Vec<u64> = col.iter().map(|c| u64::from_le_bytes(c.as_chunks::<8>().0[0])).collect();
                Arc::new(arrow::array::UInt64Array::from(vals))
            }
            DataType::Float64 => {
                let vals: Vec<f64> = col.iter().map(|c| f64::from_le_bytes(c.as_chunks::<8>().0[0])).collect();
                Arc::new(arrow::array::Float64Array::from(vals))
            }
            DataType::Int64 => {
                let vals: Vec<i64> = col.iter().map(|c| i64::from_le_bytes(c.as_chunks::<8>().0[0])).collect();
                Arc::new(arrow::array::Int64Array::from(vals))
            }
            DataType::Binary => {
                let mut offsets: Vec<i32> = Vec::with_capacity(n + 1);
                offsets.push(0);
                let mut data = Vec::new();
                for chunk in col {
                    data.extend_from_slice(chunk);
                    offsets.push(data.len() as i32);
                }
                Arc::new(arrow::array::BinaryArray::new(
                    arrow::buffer::OffsetBuffer::new(
                        arrow::buffer::ScalarBuffer::from(offsets),
                    ),
                    Buffer::from(data),
                    None,
                ))
            }
            DataType::Utf8 => {
                let mut offsets: Vec<i32> = Vec::with_capacity(n + 1);
                offsets.push(0);
                let mut data = Vec::new();
                for chunk in col {
                    data.extend_from_slice(chunk);
                    offsets.push(data.len() as i32);
                }
                Arc::new(arrow::array::StringArray::new(
                    arrow::buffer::OffsetBuffer::new(
                        arrow::buffer::ScalarBuffer::from(offsets),
                    ),
                    Buffer::from(data),
                    None,
                ))
            }
            other => unreachable!("no bridge mapping for {other:?}"),
            }
        })
        .collect();

    RecordBatch::try_new(Arc::new(proj.schema.clone()), arrays).expect("batch construction")
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
        let len = u32::from_be_bytes(cold[off + 1..off + 5].try_into().unwrap()) as usize;
        let val = off + 5;
        if *decl == fi {
            debug_assert!(
                matches!(f.ty, FieldType::Str | FieldType::VarInt) || len == f.width
            );
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
        FieldType::Str => unreachable!("Str columns bypass swap_be"),
        FieldType::VarInt | FieldType::Quant(_) | FieldType::Enum | FieldType::Offset(_) => {
            unreachable!("transformed kinds bypass swap_be (see logical_value)")
        }
    }
}

impl<S: KvEngine, K: KeyEncode, R: Row<Key = K>> Table<S, K, R> {
    /// Export all rows as one Arrow `RecordBatch` (ADR-0007 Phase 1).
    ///
    /// Columns: key fields (declaration order) then payload fields. All
    /// current field kinds are fixed-width and non-nullable; `String`
    /// payload fields map to `Utf8` when the variable-length regime lands.
    ///
    /// Reads through the engine's scan surface — engine-independent, same
    /// batch shape for fjall / slatedb / MockStore-backed tables.
    pub fn to_record_batch(&self) -> RecordBatch {
        let proj = Projection::<K, R>::new();
        let rows: Vec<(Vec<u8>, Vec<u8>)> = self.scan_rows_raw().into_iter().collect();
        build_batch(&proj, &rows)
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

    /// Approximate per-batch memory: one row's full field width.
    pub fn row_width(&self) -> usize {
        Projection::<K, R>::new().total_width()
    }
}
