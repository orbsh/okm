//! The bytes: varints, the part and block headers, one record, the footer.
//!
//! Every structure here is fixed-width on purpose. A reader that has lost its way
//! (a torn tail, a block that ends early) must be able to say so from the bytes
//! alone, because there is no store behind this file to rescan.

pub const PART_MAGIC: [u8; 4] = *b"OKMS";
pub const BLOCK_MAGIC: [u8; 4] = *b"OKMB";
pub const FOOTER_MAGIC: [u8; 4] = *b"OKMF";
pub const INDEX_MAGIC: [u8; 4] = *b"OKMI";
pub const DICT_FILE_MAGIC: [u8; 4] = *b"OKMD";
pub const DICT_BLOCK_MAGIC: [u8; 4] = *b"OKMK";
pub const VERSION: u8 = 1;

pub const PART_HEADER_LEN: u64 = 48;
pub const BLOCK_HEADER_LEN: u64 = 40;
pub const FOOTER_LEN: u64 = 40;
pub const INDEX_HEADER_LEN: u64 = 8;
pub const INDEX_ROW_LEN: u64 = 32;
pub const DICT_FILE_HEADER_LEN: u64 = 8;
pub const DICT_BLOCK_HEADER_LEN: u64 = 32;

/// The unit of a stored timestamp. `Ns` is the default: the cheap format is the
/// point, and a caller that over-specifies pays nothing (a delta is a varint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Precision {
    S,
    Ms,
    Us,
    Ns,
}

impl Precision {
    pub fn code(self) -> u8 {
        match self {
            Precision::S => 0,
            Precision::Ms => 1,
            Precision::Us => 2,
            Precision::Ns => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Precision::S,
            1 => Precision::Ms,
            2 => Precision::Us,
            3 => Precision::Ns,
            _ => return None,
        })
    }

    /// How many of these units make a second — `10`, `1e3`, `1e6`, `1e9`.
    pub fn per_second(self) -> u64 {
        match self {
            Precision::S => 1,
            Precision::Ms => 1_000,
            Precision::Us => 1_000_000,
            Precision::Ns => 1_000_000_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Precision::S => "s",
            Precision::Ms => "ms",
            Precision::Us => "us",
            Precision::Ns => "ns",
        }
    }
}

pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub fn get_varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        if shift >= 64 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
}

pub fn put_svarint(out: &mut Vec<u8>, value: i64) {
    put_varint(out, ((value << 1) ^ (value >> 63)) as u64);
}

pub fn get_svarint(bytes: &[u8], pos: &mut usize) -> Option<i64> {
    let raw = get_varint(bytes, pos)?;
    Some(((raw >> 1) as i64) ^ -((raw & 1) as i64))
}

/// The fixed part header. Written once, never patched: a part is pure append, and
/// that is what lets a crash only ever damage the tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartHeader {
    pub precision: Precision,
    pub bucket: u64,
    /// Timestamp of the first record, in the declared precision.
    pub base_ts: u64,
    /// Logical record number of the first record.
    pub first_offset: u64,
    pub created_unix_ms: u64,
}

impl PartHeader {
    pub fn encode(&self) -> [u8; PART_HEADER_LEN as usize] {
        let mut out = [0u8; PART_HEADER_LEN as usize];
        out[0..4].copy_from_slice(&PART_MAGIC);
        out[4] = VERSION;
        out[5] = self.precision.code();
        out[8..16].copy_from_slice(&self.bucket.to_be_bytes());
        out[16..24].copy_from_slice(&self.base_ts.to_be_bytes());
        out[24..32].copy_from_slice(&self.first_offset.to_be_bytes());
        out[32..40].copy_from_slice(&self.created_unix_ms.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < PART_HEADER_LEN as usize || bytes[0..4] != PART_MAGIC {
            return None;
        }
        if bytes[4] != VERSION {
            return None;
        }
        Some(PartHeader {
            precision: Precision::from_code(bytes[5])?,
            bucket: u64::from_be_bytes(bytes[8..16].try_into().ok()?),
            base_ts: u64::from_be_bytes(bytes[16..24].try_into().ok()?),
            first_offset: u64::from_be_bytes(bytes[24..32].try_into().ok()?),
            created_unix_ms: u64::from_be_bytes(bytes[32..40].try_into().ok()?),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Raw,
    Zstd,
}

impl Codec {
    pub fn code(self) -> u8 {
        match self {
            Codec::Raw => 0,
            Codec::Zstd => 1,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Codec::Raw,
            1 => Codec::Zstd,
            _ => return None,
        })
    }
}

/// One compressed block: records are walked inside the decompressed payload, so the
/// block header carries what a seek needs without opening the payload at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub codec: Codec,
    pub records: u32,
    pub len_c: u32,
    pub len_u: u32,
    pub first_ts: u64,
    pub first_offset: u64,
}

impl BlockHeader {
    pub fn encode(&self) -> [u8; BLOCK_HEADER_LEN as usize] {
        let mut out = [0u8; BLOCK_HEADER_LEN as usize];
        out[0..4].copy_from_slice(&BLOCK_MAGIC);
        out[4] = self.codec.code();
        out[8..12].copy_from_slice(&self.records.to_be_bytes());
        out[12..16].copy_from_slice(&self.len_c.to_be_bytes());
        out[16..20].copy_from_slice(&self.len_u.to_be_bytes());
        out[24..32].copy_from_slice(&self.first_ts.to_be_bytes());
        out[32..40].copy_from_slice(&self.first_offset.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < BLOCK_HEADER_LEN as usize || bytes[0..4] != BLOCK_MAGIC {
            return None;
        }
        Some(BlockHeader {
            codec: Codec::from_code(bytes[4])?,
            records: u32::from_be_bytes(bytes[8..12].try_into().ok()?),
            len_c: u32::from_be_bytes(bytes[12..16].try_into().ok()?),
            len_u: u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            first_ts: u64::from_be_bytes(bytes[24..32].try_into().ok()?),
            first_offset: u64::from_be_bytes(bytes[32..40].try_into().ok()?),
        })
    }
}

/// Appended at seal: the totals a reader wants before it opens anything, and the one
/// digest that a copy or an export can verify in a single hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Footer {
    pub records: u64,
    pub last_ts: u64,
    pub blocks: u32,
    pub digest: u64,
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_LEN as usize] {
        let mut out = [0u8; FOOTER_LEN as usize];
        out[0..4].copy_from_slice(&FOOTER_MAGIC);
        out[4] = VERSION;
        out[8..16].copy_from_slice(&self.records.to_be_bytes());
        out[16..24].copy_from_slice(&self.last_ts.to_be_bytes());
        out[24..28].copy_from_slice(&self.blocks.to_be_bytes());
        out[32..40].copy_from_slice(&self.digest.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FOOTER_LEN as usize || bytes[0..4] != FOOTER_MAGIC {
            return None;
        }
        if bytes[4] != VERSION {
            return None;
        }
        Some(Footer {
            records: u64::from_be_bytes(bytes[8..16].try_into().ok()?),
            last_ts: u64::from_be_bytes(bytes[16..24].try_into().ok()?),
            blocks: u32::from_be_bytes(bytes[24..28].try_into().ok()?),
            digest: u64::from_be_bytes(bytes[32..40].try_into().ok()?),
        })
    }
}

/// One index row: where a block starts, what record number it starts with, and how
/// big it is. Appended when the block is written, never patched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexRow {
    pub first_ts: u64,
    pub first_offset: u64,
    pub file_offset: u32,
    pub len_c: u32,
    pub len_u: u32,
    pub records: u32,
}

impl IndexRow {
    pub fn encode(&self) -> [u8; INDEX_ROW_LEN as usize] {
        let mut out = [0u8; INDEX_ROW_LEN as usize];
        out[0..8].copy_from_slice(&self.first_ts.to_be_bytes());
        out[8..16].copy_from_slice(&self.first_offset.to_be_bytes());
        out[16..20].copy_from_slice(&self.file_offset.to_be_bytes());
        out[20..24].copy_from_slice(&self.len_c.to_be_bytes());
        out[24..28].copy_from_slice(&self.len_u.to_be_bytes());
        out[28..32].copy_from_slice(&self.records.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < INDEX_ROW_LEN as usize {
            return None;
        }
        Some(IndexRow {
            first_ts: u64::from_be_bytes(bytes[0..8].try_into().ok()?),
            first_offset: u64::from_be_bytes(bytes[8..16].try_into().ok()?),
            file_offset: u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            len_c: u32::from_be_bytes(bytes[20..24].try_into().ok()?),
            len_u: u32::from_be_bytes(bytes[24..28].try_into().ok()?),
            records: u32::from_be_bytes(bytes[28..32].try_into().ok()?),
        })
    }

    pub fn from_header(header: &BlockHeader, file_offset: u64) -> Option<Self> {
        Some(IndexRow {
            first_ts: header.first_ts,
            first_offset: header.first_offset,
            file_offset: u32::try_from(file_offset).ok()?,
            len_c: header.len_c,
            len_u: header.len_u,
            records: header.records,
        })
    }
}

pub fn index_header() -> [u8; INDEX_HEADER_LEN as usize] {
    let mut out = [0u8; INDEX_HEADER_LEN as usize];
    out[0..4].copy_from_slice(&INDEX_MAGIC);
    out[4] = VERSION;
    out
}

/// A record inside a block: `[len u32][ts delta varint][body]`, where the delta is
/// from the previous record in the part (the first one's is absolute).
pub fn put_record(out: &mut Vec<u8>, delta: u64, body: &[u8]) -> Option<()> {
    let mut tail = Vec::with_capacity(9);
    put_varint(&mut tail, delta);
    let len = u32::try_from(tail.len() + body.len()).ok()?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&tail);
    out.extend_from_slice(body);
    Some(())
}

/// Walk one record out of a block payload. `None` = the payload ends mid-record.
pub fn get_record<'a>(payload: &'a [u8], pos: &mut usize) -> Option<(u64, &'a [u8])> {
    let head = payload.get(*pos..*pos + 4)?;
    let len = u32::from_be_bytes(head.try_into().ok()?) as usize;
    *pos += 4;
    let end = pos.checked_add(len)?;
    let slice = payload.get(*pos..end)?;
    let mut at = 0usize;
    let delta = get_varint(slice, &mut at)?;
    *pos = end;
    Some((delta, &slice[at..]))
}
