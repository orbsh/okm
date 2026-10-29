//! One part: a header, a stream of compressed blocks, an index beside it, and a
//! footer when it is sealed. Nothing here is ever patched in place.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use xxhash_rust::xxh64::Xxh64;

use crate::Error;
use crate::body::Body;
use crate::dict::{Dict, DictConfig, dict_path_for};
use crate::format::{
    BLOCK_HEADER_LEN, BlockHeader, Codec, FOOTER_LEN, Footer, INDEX_HEADER_LEN, INDEX_ROW_LEN,
    IndexRow, PART_HEADER_LEN, PartHeader, Precision, get_record, put_record,
};

/// How a compressed block is produced. Level 1 is the default: a log's blocks are
/// written on the hot path, and the ratio at level 1 versus level 6 does not pay for
/// the CPU when the same bytes are recompressed properly in the lake.
pub const DEFAULT_ZSTD_LEVEL: i32 = 1;

pub struct PartWriter {
    path: PathBuf,
    file: File,
    index: File,
    header: PartHeader,
    block_bytes: usize,
    level: i32,
    /// `Raw` skips compression entirely; otherwise the block is compressed and kept
    /// only when that made it smaller.
    codec: Codec,
    /// Offsets inside this part are logical record numbers, not bytes.
    next_offset: u64,
    last_ts: Option<u64>,
    file_off: u64,
    block: Vec<u8>,
    block_first_ts: u64,
    block_first_offset: u64,
    block_records: u32,
    records: u64,
    blocks: u32,
    digest: Xxh64,
    dict: Option<Dict>,
}

impl PartWriter {
    /// Start a part and write its header. The name is `.open`: the file says what
    /// state it is in, so sealing is a rename and not a flag inside the bytes.
    pub fn create(
        path: &Path,
        header: PartHeader,
        block_bytes: usize,
        level: i32,
        codec: Codec,
        dict: Option<&DictConfig>,
    ) -> Result<Self, Error> {
        let dict = match dict {
            Some(config) => Some(Dict::create(&dict_path_for(path)?, config, level)?),
            None => None,
        };
        let index_path = index_path_for(path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(path)?;
        let mut writer = PartWriter {
            path: path.to_path_buf(),
            file,
            index: File::create(&index_path)?,
            header,
            block_bytes,
            level,
            codec,
            next_offset: header.first_offset,
            last_ts: None,
            file_off: 0,
            block: Vec::with_capacity(block_bytes),
            block_first_ts: 0,
            block_first_offset: header.first_offset,
            block_records: 0,
            records: 0,
            blocks: 0,
            digest: Xxh64::new(0),
            dict,
        };
        writer.file.write_all(&header.encode())?;
        writer.file_off = PART_HEADER_LEN;
        writer.index.write_all(&crate::format::index_header())?;
        Ok(writer)
    }

    /// Take over an existing `.open` part: validate what is there, cut a torn tail,
    /// and rebuild the index from the blocks that survived. This is the whole crash
    /// story — no checksum, because a crash can only truncate the tail, and a record
    /// whose length runs past the end of the file is exactly the one that was being
    /// written.
    pub fn adopt(
        path: &Path,
        block_bytes: usize,
        level: i32,
        codec: Codec,
        dict: Option<&DictConfig>,
    ) -> Result<Self, Error> {
        let dict = match dict {
            Some(config) => Some(Dict::adopt(&dict_path_for(path)?, config, level)?),
            None => None,
        };
        let index_path = index_path_for(path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut head = vec![0u8; PART_HEADER_LEN as usize];
        file.read_exact(&mut head)?;
        let header = PartHeader::decode(&head).ok_or(Error::BadFrame("part header"))?;

        let mut file_off = PART_HEADER_LEN;
        let mut next_offset = header.first_offset;
        let mut last_ts: Option<u64> = None;
        let mut records = 0u64;
        let mut blocks = 0u32;
        let mut digest = Xxh64::new(0);
        let mut rows: Vec<IndexRow> = Vec::new();

        let mut head_buf = [0u8; BLOCK_HEADER_LEN as usize];
        loop {
            if file.seek(SeekFrom::Start(file_off)).is_err() {
                break;
            }
            if read_full(&mut file, &mut head_buf).is_err() {
                break;
            }
            let block = match BlockHeader::decode(&head_buf) {
                Some(block) => block,
                None => break,
            };
            if block.records == 0 || block.len_c == 0 {
                break;
            }
            let mut payload = vec![0u8; block.len_c as usize];
            if read_full(&mut file, &mut payload).is_err() {
                break;
            }
            let raw = match decode_payload(&payload, &block) {
                Ok(raw) => raw,
                Err(_) => break,
            };
            if raw.len() != block.len_u as usize {
                break;
            }
            // The payload is intact if every record inside it can be walked out.
            let mut pos = 0usize;
            let mut walked = 0u32;
            while pos < raw.len() {
                match get_record(&raw, &mut pos) {
                    Some(_) => walked += 1,
                    None => break,
                }
            }
            if walked != block.records {
                break;
            }
            digest.update(&raw);
            rows.push(IndexRow::from_header(&block, file_off).ok_or(Error::Overflow)?);
            next_offset = block.first_offset + u64::from(block.records);
            last_ts = last_ts_of(&raw, block.first_ts)?;
            records += u64::from(block.records);
            blocks += 1;
            file_off += BLOCK_HEADER_LEN + u64::from(block.len_c);
        }

        // Cut whatever follows the last good block: a torn tail, or a footer that was
        // half written when the process died.
        let len = file.metadata()?.len();
        if len > file_off {
            file.set_len(file_off)?;
            file.sync_all()?;
        }

        let index = rebuild_index(&index_path, &rows)?;
        Ok(PartWriter {
            path: path.to_path_buf(),
            file,
            index,
            header,
            block_bytes,
            level,
            codec,
            next_offset,
            last_ts,
            file_off,
            block: Vec::with_capacity(block_bytes),
            block_first_ts: 0,
            block_first_offset: next_offset,
            block_records: 0,
            records,
            blocks,
            digest,
            dict,
        })
    }

    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    pub fn header(&self) -> PartHeader {
        self.header
    }

    pub fn last_ts(&self) -> Option<u64> {
        self.last_ts
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Append one body at `ts`, returning its offset.
    ///
    /// Offsets belong to the STREAM, which is a partition (or the log itself when no
    /// partition is declared) — not to the whole log. A block header carries only the
    /// first offset, and the reader walks a block's records by counting up from it, so a
    /// part whose records were interleaved with another partition's could not be read
    /// back. One counter per stream, never shared.
    pub fn append(&mut self, ts: u64, body: &Body) -> Result<u64, Error> {
        let encoded = body.encode_with(self.dict.as_mut());
        let delta = match self.last_ts {
            None => {
                if self.header.base_ts == 0 && self.records == 0 {
                    self.header.base_ts = ts;
                }
                ts
            }
            Some(last) => {
                if ts < last {
                    return Err(Error::TimeBackwards { last, given: ts });
                }
                ts - last
            }
        };
        if self.block.is_empty() {
            self.block_first_ts = ts;
            self.block_first_offset = self.next_offset;
        }
        put_record(&mut self.block, delta, &encoded).ok_or(Error::Overflow)?;
        let offset = self.next_offset;
        self.next_offset += 1;
        self.block_records += 1;
        self.last_ts = Some(ts);
        if self.block.len() >= self.block_bytes {
            self.flush_block()?;
        }
        Ok(offset)
    }

    pub fn pending_bytes(&self) -> usize {
        self.block.len()
    }

    /// Compress the buffered block, write it, and record where it went. Compression
    /// happens here — once per block, not once per record — and the same pass feeds
    /// the part digest.
    pub fn flush_block(&mut self) -> Result<(), Error> {
        if self.block_records == 0 {
            return Ok(());
        }
        // The dictionary goes to disk first: a block of records may refer to strings
        // whose block is still in memory, and a reader that opened the part meanwhile
        // would have nothing to resolve them with.
        if let Some(dict) = self.dict.as_mut() {
            dict.flush_if_dirty()?;
        }
        let raw = std::mem::take(&mut self.block);
        let (codec, payload) = match self.codec {
            Codec::Raw => (Codec::Raw, raw.clone()),
            Codec::Zstd => {
                let compressed =
                    zstd::bulk::compress(&raw, self.level).map_err(|_| Error::Codec)?;
                if compressed.len() < raw.len() {
                    (Codec::Zstd, compressed)
                } else {
                    (Codec::Raw, raw.clone())
                }
            }
        };
        let block = BlockHeader {
            codec,
            records: self.block_records,
            len_c: u32::try_from(payload.len()).map_err(|_| Error::Overflow)?,
            len_u: u32::try_from(raw.len()).map_err(|_| Error::Overflow)?,
            first_ts: self.block_first_ts,
            first_offset: self.block_first_offset,
        };
        self.file.seek(SeekFrom::Start(self.file_off))?;
        self.file.write_all(&block.encode())?;
        self.file.write_all(&payload)?;
        self.file.sync_data()?;
        self.records += u64::from(block.records);
        self.blocks += 1;
        self.digest.update(&raw);
        let row = IndexRow::from_header(&block, self.file_off).ok_or(Error::Overflow)?;
        // The index is DERIVED data: adoption rebuilds it from the block headers, so it
        // is not fsynced per block. That halves the syncs on the write path, and the
        // measurement says each sync pair cost about a millisecond.
        self.index.write_all(&row.encode())?;
        self.file_off += BLOCK_HEADER_LEN + u64::from(block.len_c);
        self.block = Vec::with_capacity(self.block_bytes);
        self.block_records = 0;
        self.block_first_offset = self.next_offset;
        Ok(())
    }

    pub fn written_bytes(&self) -> u64 {
        self.file_off
    }

    /// Close the part: flush, write the footer, fsync, then rename `.open` to `.log`.
    /// The rename is the seal — a reader that sees `.log` knows the footer is there.
    pub fn seal(mut self) -> Result<Footer, Error> {
        self.flush_block()?;
        if let Some(dict) = self.dict.as_mut() {
            dict.finish()?;
        }
        let footer = Footer {
            records: self.records,
            last_ts: self.last_ts.unwrap_or(0),
            blocks: self.blocks,
            digest: self.digest.digest(),
        };
        self.file.seek(SeekFrom::Start(self.file_off))?;
        self.file.write_all(&footer.encode())?;
        self.file.sync_all()?;
        self.index.sync_all()?;
        drop(self.index);
        let sealed = sealed_path_for(&self.path)?;
        std::fs::rename(&self.path, &sealed)?;
        sync_dir(self.path.parent().unwrap_or(Path::new(".")))?;
        Ok(footer)
    }
}

/// A part on disk, opened for reading.
pub struct PartReader {
    pub path: PathBuf,
    pub header: PartHeader,
    pub footer: Option<Footer>,
    rows: Vec<IndexRow>,
    sealed: bool,
    dict: Option<Dict>,
    /// The first and last timestamp this part covers. Computed at open, because an open
    /// part has no footer and its last block's `first_ts` is not the last record's.
    span: Option<(u64, u64)>,
}

impl PartReader {
    pub fn open(path: &Path, dict: Option<&DictConfig>) -> Result<Self, Error> {
        let dict = match dict {
            Some(config) => {
                let dict_path = dict_path_for(path)?;
                if dict_path.exists() {
                    Some(Dict::load(&dict_path, config, DEFAULT_ZSTD_LEVEL)?)
                } else {
                    None
                }
            }
            None => None,
        };
        let mut file = File::open(path)?;
        let mut head = vec![0u8; PART_HEADER_LEN as usize];
        file.read_exact(&mut head)?;
        let header = PartHeader::decode(&head).ok_or(Error::BadFrame("part header"))?;
        let sealed = path.extension().map(|e| e == "log").unwrap_or(false);
        let footer = if sealed {
            read_footer(&mut file)?
        } else {
            None
        };
        let rows = read_index(&index_path_for(path)?)?;
        let span = match (rows.first(), footer) {
            (None, _) => None,
            (Some(first), Some(footer)) => Some((first.first_ts, footer.last_ts)),
            (Some(first), None) => {
                // An open part: the truth about its last record is in its last block.
                let last_row = rows.last().ok_or(Error::BadFrame("index"))?;
                let mut block_raw = vec![0u8; last_row.len_c as usize];
                file.seek(SeekFrom::Start(u64::from(last_row.file_offset)))?;
                let mut head = [0u8; BLOCK_HEADER_LEN as usize];
                read_full(&mut file, &mut head)?;
                let block = BlockHeader::decode(&head).ok_or(Error::BadFrame("block header"))?;
                read_full(&mut file, &mut block_raw)?;
                let raw = decode_payload(&block_raw, &block)?;
                let last_ts = last_ts_of(&raw, block.first_ts)?.unwrap_or(block.first_ts);
                Some((first.first_ts, last_ts))
            }
        };
        Ok(PartReader {
            path: path.to_path_buf(),
            header,
            footer,
            rows,
            sealed,
            dict,
            span,
        })
    }

    pub fn sealed(&self) -> bool {
        self.sealed
    }

    /// Records this part covers, as `[first_offset, end)`.
    pub fn offset_span(&self) -> (u64, u64) {
        let start = self.header.first_offset;
        let end = match self.footer {
            Some(footer) => start + footer.records,
            None => match self.rows.last() {
                Some(row) => row.first_offset + u64::from(row.records),
                None => start,
            },
        };
        (start, end)
    }

    pub fn ts_span(&self) -> Option<(u64, u64)> {
        self.span
    }

    pub fn rows(&self) -> &[IndexRow] {
        self.rows.as_slice()
    }

    /// Read records from logical offset `from`, at most `max`.
    pub fn read_from(
        &self,
        from: u64,
        max: usize,
        out: &mut Vec<(u64, u64, Body)>,
    ) -> Result<(), Error> {
        let mut file = File::open(&self.path)?;
        let start = self.row_index_for_offset(from);
        for row in &self.rows[start..] {
            if out.len() >= max {
                break;
            }
            let (block, raw) = self.read_block(&mut file, row)?;
            scan_block(
                &block,
                &raw,
                Select::Offset { from },
                max,
                self.dict.as_ref(),
                out,
            )?;
        }
        Ok(())
    }

    /// Read every record whose timestamp is in `[from_ts, to_ts]`, at most `max`. The
    /// index rows are in time order, so this is a binary search plus a scan.
    pub fn read_by_time(
        &self,
        from_ts: u64,
        to_ts: u64,
        max: usize,
        out: &mut Vec<(u64, u64, Body)>,
    ) -> Result<(), Error> {
        let mut file = File::open(&self.path)?;
        let start = self
            .rows
            .partition_point(|row| row.first_ts < from_ts)
            .saturating_sub(1);
        for row in &self.rows[start..] {
            if out.len() >= max || row.first_ts > to_ts {
                break;
            }
            let (block, raw) = self.read_block(&mut file, row)?;
            let last = scan_block(
                &block,
                &raw,
                Select::Time {
                    from: from_ts,
                    to: to_ts,
                },
                max,
                self.dict.as_ref(),
                out,
            )?;
            if last > to_ts {
                break;
            }
        }
        Ok(())
    }

    fn row_index_for_offset(&self, from: u64) -> usize {
        self.rows
            .partition_point(|row| row.first_offset <= from)
            .saturating_sub(1)
    }

    fn read_block(&self, file: &mut File, row: &IndexRow) -> Result<(BlockHeader, Vec<u8>), Error> {
        file.seek(SeekFrom::Start(u64::from(row.file_offset)))?;
        let mut head = [0u8; BLOCK_HEADER_LEN as usize];
        read_full(file, &mut head)?;
        let block = BlockHeader::decode(&head).ok_or(Error::BadFrame("block header"))?;
        let mut payload = vec![0u8; block.len_c as usize];
        read_full(file, &mut payload)?;
        let raw = decode_payload(&payload, &block)?;
        Ok((block, raw))
    }
}

/// What a scan is looking for: a logical offset (sequential reads) or a time range.
#[derive(Clone, Copy, Debug)]
enum Select {
    Offset { from: u64 },
    Time { from: u64, to: u64 },
}

/// Walk one decompressed block, pushing the records the selection wants. Returns the
/// timestamp of the last record walked, so a time scan can stop at the block that
/// crosses its upper bound.
///
/// The first record of a block takes its timestamp from the block header, not from
/// its own delta: that delta is relative to the last record of the PREVIOUS block,
/// which is exactly the byte a seek did not read.
fn scan_block(
    block: &BlockHeader,
    raw: &[u8],
    select: Select,
    max: usize,
    dict: Option<&Dict>,
    out: &mut Vec<(u64, u64, Body)>,
) -> Result<u64, Error> {
    let mut pos = 0usize;
    let mut offset = block.first_offset;
    let mut ts = block.first_ts;
    let mut first = true;
    while pos < raw.len() {
        let (delta, body) = match get_record(raw, &mut pos) {
            Some(record) => record,
            None => break,
        };
        if !first {
            ts = ts.saturating_add(delta);
        }
        let wanted = match select {
            Select::Offset { from } => offset >= from,
            Select::Time { from, to } => ts >= from && ts <= to,
        };
        if wanted && out.len() < max {
            let decoded = Body::decode_with(body, dict).ok_or(Error::BadFrame("body"))?;
            out.push((ts, offset, decoded));
        }
        offset += 1;
        first = false;
    }
    Ok(ts)
}

fn last_ts_of(raw: &[u8], first_ts: u64) -> Result<Option<u64>, Error> {
    let mut pos = 0usize;
    let mut ts = first_ts;
    let mut last = None;
    let mut first = true;
    while pos < raw.len() {
        let (delta, _) = get_record(raw, &mut pos).ok_or(Error::BadFrame("record"))?;
        if !first {
            ts += delta;
        }
        last = Some(ts);
        first = false;
    }
    Ok(last)
}

fn decode_payload(payload: &[u8], block: &BlockHeader) -> Result<Vec<u8>, Error> {
    match block.codec {
        Codec::Raw => Ok(payload.to_vec()),
        Codec::Zstd => {
            zstd::bulk::decompress(payload, block.len_u as usize).map_err(|_| Error::Codec)
        }
    }
}

fn read_full(file: &mut File, buf: &mut [u8]) -> Result<(), Error> {
    file.read_exact(buf).map_err(Error::Io)
}

fn read_footer(file: &mut File) -> Result<Option<Footer>, Error> {
    let len = file.metadata()?.len();
    if len < FOOTER_LEN + PART_HEADER_LEN {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(len - FOOTER_LEN))?;
    let mut buf = [0u8; FOOTER_LEN as usize];
    read_full(file, &mut buf)?;
    Ok(Footer::decode(&buf))
}

fn read_index(path: &Path) -> Result<Vec<IndexRow>, Error> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(Vec::new()),
    };
    let mut header = [0u8; INDEX_HEADER_LEN as usize];
    if read_full(&mut file, &mut header).is_err() {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    let mut row = [0u8; INDEX_ROW_LEN as usize];
    while read_full(&mut file, &mut row).is_ok() {
        match IndexRow::decode(&row) {
            Some(decoded) => rows.push(decoded),
            None => break,
        }
    }
    Ok(rows)
}

fn rebuild_index(path: &Path, rows: &[IndexRow]) -> Result<File, Error> {
    let mut file = File::create(path)?;
    file.write_all(&crate::format::index_header())?;
    for row in rows {
        file.write_all(&row.encode())?;
    }
    file.sync_all()?;
    Ok(file)
}

pub fn index_path_for(part: &Path) -> Result<PathBuf, Error> {
    let stem = part.file_stem().ok_or(Error::BadPath)?;
    let mut name = stem.to_os_string();
    name.push(".idx");
    Ok(part.with_file_name(name))
}

pub fn sealed_path_for(part: &Path) -> Result<PathBuf, Error> {
    let stem = part.file_stem().ok_or(Error::BadPath)?;
    let mut name = stem.to_os_string();
    name.push(".log");
    Ok(part.with_file_name(name))
}

pub fn open_path(dir: &Path, number: u32) -> PathBuf {
    dir.join(format!("part-{number:08}.open"))
}

pub fn sync_dir(dir: &Path) -> Result<(), Error> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Part numbers in a bucket directory, sealed or open.
pub fn part_numbers(dir: &Path) -> Result<Vec<u32>, Error> {
    let mut numbers = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(numbers),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let rest = match name.strip_prefix("part-") {
            Some(rest) => rest,
            None => continue,
        };
        let digits = match rest.split('.').next() {
            Some(digits) => digits,
            None => continue,
        };
        if !(name.ends_with(".open") || name.ends_with(".log")) {
            continue;
        }
        if let Ok(number) = digits.parse::<u32>() {
            numbers.push(number);
        }
    }
    numbers.sort_unstable();
    numbers.dedup();
    Ok(numbers)
}

pub fn sealed_parts(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut parts = Vec::new();
    for number in part_numbers(dir)? {
        let sealed = dir.join(format!("part-{number:08}.log"));
        if sealed.exists() {
            parts.push(sealed);
        }
    }
    Ok(parts)
}

pub fn open_part(dir: &Path) -> Result<Option<PathBuf>, Error> {
    for number in part_numbers(dir)? {
        let open = dir.join(format!("part-{number:08}.open"));
        if open.exists() {
            return Ok(Some(open));
        }
    }
    Ok(None)
}

pub fn precision_of(header: &PartHeader) -> Precision {
    header.precision
}
