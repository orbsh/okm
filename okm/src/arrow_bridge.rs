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
    // Payload half: walk each row's TLV frames in declaration order and
    // take the value bytes of the matching field (skipping tag + len).
    // Fixed-width fields sit at a computable static offset; `Str` fields
    // must be walked frame-by-frame because every preceding variable-length
    // field shifts the offset.
    let has_var = proj.row_fields.iter().any(|f| f.ty == FieldType::Str);
    for (fi, f) in proj.row_fields.iter().enumerate() {
        let mut col: Vec<Vec<u8>> = Vec::with_capacity(n);
        for (_, v) in rows {
            if has_var {
                let val = tlv_value(v, proj.row_fields, fi);
                if f.ty == FieldType::Str {
                    col.push(val.to_vec());
                } else {
                    col.push(swap_be(val, 0, f.width, f.ty));
                }
            } else {
                let off = tlv_value_offset(proj.row_fields, fi);
                col.push(swap_be(v, off, f.width, f.ty));
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

/// Byte offset of field `fi`'s value inside a TLV payload: walk frames
/// 0..fi (tag u8 + len u32 + value), summing their strides. Declared field
/// order == TLV emit order, so strides align with the wire format.
///
/// Only valid when no field before `fi` is variable-length — with `Str`
/// fields present, use [`tlv_value`] instead (dynamic walk).
fn tlv_value_offset(fields: &[FieldDesc], fi: usize) -> usize {
    // prior frames (tag+len+value) plus this frame's own tag+len header
    5 + fields[..fi].iter().map(|f| 5 + f.width).sum::<usize>()
}

/// Value slice of field `fi` inside a TLV payload, walked frame-by-frame.
/// Handles variable-length (`Str`) frames whose stride is the frame's own
/// `len`. Returns the value region only (tag + len skipped).
fn tlv_value<'a>(payload: &'a [u8], fields: &[FieldDesc], fi: usize) -> &'a [u8] {
    let mut off = 0usize;
    for (i, f) in fields.iter().enumerate() {
        let len = u32::from_be_bytes(payload[off + 1..off + 5].try_into().unwrap()) as usize;
        let val = off + 5;
        if i == fi {
            debug_assert!(f.ty == FieldType::Str || len == f.width);
            return &payload[val..val + len];
        }
        off = val + len;
    }
    unreachable!("field index out of TLV frame range: {fi}");
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
