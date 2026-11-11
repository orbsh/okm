//! Where a file's keys START and END — the sidecar a fold prunes with.
//!
//! Two files can only share a key if their ranges overlap, so a fold that must resolve one
//! key can skip every file whose range does not contain it. The bounds are computed when a
//! file is written and recorded in its manifest, which is why deciding what to fold stops
//! being a read of every key in the table.
//!
//! Values are encoded as SORTABLE bytes — big-endian integers with the sign bit flipped for
//! the signed one, raw UTF-8 for strings — so one `Vec<u8>` ordering answers every declared
//! key type without the reader knowing which type it is looking at. A type outside that set
//! gets no bound at all, and a column without a bound cannot prune anything: an over-wide
//! candidate list is a slow fold, a wrong one loses rows.

use arrow::array::{
    Array, BinaryArray, BooleanArray, FixedSizeBinaryArray, Int64Array, StringArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::DataType;

/// One key column's range in one file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bound {
    pub column: String,
    pub min: Vec<u8>,
    pub max: Vec<u8>,
}

/// The bounds of every prunable key column of a file, in the log's schema order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bounds(pub Vec<Bound>);

impl Bounds {
    /// Do two files' ranges overlap on EVERY column they both bound? Only overlapping ranges
    /// can share a key.
    pub fn overlaps(&self, other: &Bounds) -> bool {
        self.0.iter().all(|mine| {
            other
                .0
                .iter()
                .find(|theirs| theirs.column == mine.column)
                .is_none_or(|theirs| mine.min <= theirs.max && theirs.min <= mine.max)
        })
    }

    /// The range on one column, if this file has it.
    pub fn on(&self, column: &str) -> Option<&Bound> {
        self.0.iter().find(|b| b.column == column)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The bounds of the given key columns in one batch. A column whose type has no sortable
/// encoding is left out (and then prunes nothing).
pub fn of_batch(batch: &arrow::array::RecordBatch, key_names: &[String]) -> Result<Bounds, String> {
    let mut out: Vec<Bound> = Vec::new();
    for name in key_names {
        let column = batch
            .column_by_name(name)
            .ok_or_else(|| format!("no key column `{name}`"))?;
        // An empty batch has no range: nothing to prune with, and indexing row 0 would be a
        // panic dressed up as an optimisation.
        if column.is_empty() {
            continue;
        }
        let Some(first) = sortable(column.as_ref(), 0) else {
            continue;
        };
        let mut min = first.clone();
        let mut max = first;
        for row in 1..column.len() {
            let Some(value) = sortable(column.as_ref(), row) else {
                continue;
            };
            if value < min {
                min = value.clone();
            }
            if value > max {
                max = value;
            }
        }
        out.push(Bound {
            column: name.clone(),
            min,
            max,
        });
    }
    Ok(Bounds(out))
}

/// One value's sortable bytes, or `None` for a null and for a type outside the encoding.
fn sortable(column: &dyn Array, row: usize) -> Option<Vec<u8>> {
    if column.is_null(row) {
        return None;
    }
    macro_rules! unsigned {
        ($ty:ty) => {{
            let values = column.as_any().downcast_ref::<$ty>()?;
            Some(values.value(row).to_be_bytes().to_vec())
        }};
    }
    match column.data_type() {
        DataType::UInt8 => unsigned!(UInt8Array),
        DataType::UInt16 => unsigned!(UInt16Array),
        DataType::UInt32 => unsigned!(UInt32Array),
        DataType::UInt64 => unsigned!(UInt64Array),
        DataType::Int64 => {
            let values = column.as_any().downcast_ref::<Int64Array>()?;
            // Two's complement does not sort as signed numbers; flipping the sign bit makes
            // big-endian bytes sort the way the values do.
            let lifted = (values.value(row) as u64) ^ (1u64 << 63);
            Some(lifted.to_be_bytes().to_vec())
        }
        DataType::Boolean => {
            let values = column.as_any().downcast_ref::<BooleanArray>()?;
            Some(vec![u8::from(values.value(row))])
        }
        DataType::Utf8 => {
            let values = column.as_any().downcast_ref::<StringArray>()?;
            Some(values.value(row).as_bytes().to_vec())
        }
        DataType::Binary => {
            let values = column.as_any().downcast_ref::<BinaryArray>()?;
            Some(values.value(row).to_vec())
        }
        DataType::FixedSizeBinary(_) => {
            let values = column.as_any().downcast_ref::<FixedSizeBinaryArray>()?;
            Some(values.value(row).to_vec())
        }
        // Date/time, decimal, uuid as text, a nested shape: no ordering this sidecar can
        // promise, so no bound — the file stays a fold candidate.
        _ => None,
    }
}

/// Hex, for a manifest that has to be JSON.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The inverse, for reading a manifest back.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let text = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(text, 16).ok()?);
    }
    Some(out)
}
/// The manifest's shape for these bounds: `(column, min hex, max hex)`, because a manifest is
/// JSON and a byte string is not.
pub fn to_manifest(bounds: &Bounds) -> Vec<(String, String, String)> {
    bounds
        .0
        .iter()
        .map(|b| (b.column.clone(), to_hex(&b.min), to_hex(&b.max)))
        .collect()
}

/// The inverse: a manifest's bounds, skipping entries whose hex does not parse (a bound this
/// writer cannot read prunes nothing, and pruning nothing is safe).
pub fn from_manifest(entries: &[(String, String, String)]) -> Bounds {
    Bounds(
        entries
            .iter()
            .filter_map(|(column, min, max)| {
                Some(Bound {
                    column: column.clone(),
                    min: from_hex(min)?,
                    max: from_hex(max)?,
                })
            })
            .collect(),
    )
}
