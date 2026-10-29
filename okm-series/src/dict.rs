//! The dictionary: strings that earned a code, in blocks that double their width.
//!
//! Promotion is by tiers of the string's own width (ADR-0046 §5): a string no longer
//! than its code is never interned, and longer strings earn a code sooner the longer
//! they are. The thresholds are configuration, not format — changing them changes what
//! FUTURE parts promote, and a reader never needs to know.
//!
//! Blocks are written once, complete: entries accumulate in memory and land on disk
//! when the block fills (or at seal). That keeps the file pure append — no count field
//! is ever patched — which is the same reason the part has no in-place write.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::Error;
use crate::format::{
    Codec, DICT_BLOCK_HEADER_LEN, DICT_BLOCK_MAGIC, DICT_FILE_HEADER_LEN, DICT_FILE_MAGIC, VERSION,
    get_varint, put_varint,
};

/// `[[4, 10], [8, 5], [16, 3], [0, 2]]`: multiples of the code width, and the count at
/// which a string of that length earns a code. `0` means "no upper bound".
pub const DEFAULT_TIERS: [(u32, u32); 4] = [(4, 10), (8, 5), (16, 3), (0, 2)];

/// How many entries one block holds before the chain widens.
pub const DEFAULT_BLOCK_ENTRIES: u32 = 1 << 16;

#[derive(Clone, Debug)]
pub struct DictConfig {
    pub tiers: Vec<(u32, u32)>,
    pub block_entries: u32,
}

impl Default for DictConfig {
    fn default() -> Self {
        DictConfig {
            tiers: DEFAULT_TIERS.to_vec(),
            block_entries: DEFAULT_BLOCK_ENTRIES,
        }
    }
}

/// The width of a block's ids, from its code. Codes go up, never down.
fn width_of(code: u8) -> Option<usize> {
    Some(match code {
        0 => 2,
        1 => 4,
        2 => 8,
        _ => return None,
    })
}

/// The kind byte a string in `block` is written with: `DICT_BASE + block`.
fn code_of(block: usize) -> u8 {
    let base = crate::body::DICT_BASE;
    let index = u8::try_from(block).unwrap_or(u8::MAX - base);
    base.saturating_add(index)
}

#[derive(Clone, Debug)]
struct Block {
    width: usize,
    entries: Vec<String>,
    /// Where the block starts in the dict file; 0 while it is still in memory.
    file_offset: u64,
}

pub struct Dict {
    path: PathBuf,
    file: Option<File>,
    file_offset: u64,
    blocks: Vec<Block>,
    open: usize,
    counts: HashMap<String, u32>,
    promoted: HashMap<String, (u8, u64)>,
    tiers: Vec<(u32, u32)>,
    block_entries: u32,
    level: i32,
}

impl Dict {
    /// A dictionary for a new part. Nothing is written until a block fills.
    pub fn create(path: &Path, config: &DictConfig, level: i32) -> Result<Self, Error> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(path)?;
        let mut header = [0u8; DICT_FILE_HEADER_LEN as usize];
        header[0..4].copy_from_slice(&DICT_FILE_MAGIC);
        header[4] = VERSION;
        file.write_all(&header)?;
        file.sync_all()?;
        write_tail(path, 0)?;
        Ok(Dict {
            path: path.to_path_buf(),
            file: Some(file),
            file_offset: DICT_FILE_HEADER_LEN,
            blocks: vec![Block {
                width: 2,
                entries: Vec::new(),
                file_offset: 0,
            }],
            open: 0,
            counts: HashMap::new(),
            promoted: HashMap::new(),
            tiers: config.tiers.clone(),
            block_entries: config.block_entries.max(1),
            level,
        })
    }

    /// Take over the dictionary of an existing part: the blocks on disk are the state,
    /// so the ids a record referred to before the crash still mean the same string.
    pub fn adopt(path: &Path, config: &DictConfig, level: i32) -> Result<Self, Error> {
        let mut dict = Dict::load(path, config, level)?;
        let file = OpenOptions::new().write(true).read(true).open(path)?;
        dict.file = Some(file);
        // Whatever is on disk is closed: a block that already carries a count must not
        // grow. Start a fresh block at the width the last one had, and let it widen only
        // when it fills.
        let width = dict.blocks.last().map(|block| block.width).unwrap_or(2);
        dict.blocks.push(Block {
            width,
            entries: Vec::new(),
            file_offset: 0,
        });
        dict.open = dict.blocks.len() - 1;
        Ok(dict)
    }

    /// Read-only view: a reader needs no write handle.
    pub fn load(path: &Path, config: &DictConfig, level: i32) -> Result<Self, Error> {
        let mut blocks = Vec::new();
        let mut file_offset = 0u64;
        if let Ok(mut file) = File::open(path) {
            let len = file.metadata()?.len();
            if len >= DICT_FILE_HEADER_LEN {
                let mut header = [0u8; DICT_FILE_HEADER_LEN as usize];
                if file.read_exact(&mut header).is_ok() && header[0..4] == DICT_FILE_MAGIC {
                    let mut cursor = DICT_FILE_HEADER_LEN;
                    while cursor + DICT_BLOCK_HEADER_LEN <= len {
                        match read_block(&mut file, cursor) {
                            Ok(Some((block, next))) => {
                                blocks.push(Block {
                                    width: block.width,
                                    entries: block.entries,
                                    file_offset: cursor,
                                });
                                cursor = next;
                                file_offset = next;
                            }
                            _ => break,
                        }
                    }
                }
            }
        }
        let mut promoted = HashMap::new();
        for (index, block) in blocks.iter().enumerate() {
            for (id, text) in block.entries.iter().enumerate() {
                promoted.insert(text.clone(), (code_of(index), id as u64));
            }
        }
        Ok(Dict {
            path: path.to_path_buf(),
            file: None,
            file_offset,
            blocks,
            open: 0,
            counts: HashMap::new(),
            promoted,
            tiers: config.tiers.clone(),
            block_entries: config.block_entries.max(1),
            level,
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// The block a string lives in, and its id — what a reader needs to resolve a code.
    pub fn lookup(&self, block: u8, id: u64) -> Option<&str> {
        let index = block.checked_sub(crate::body::DICT_BASE)? as usize;
        let block = self.blocks.get(index)?;
        block.entries.get(id as usize).map(|text| text.as_str())
    }

    pub fn width(&self, block: u8) -> Option<usize> {
        let index = block.checked_sub(crate::body::DICT_BASE)? as usize;
        self.blocks.get(index).map(|block| block.width)
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|block| block.entries.is_empty())
    }

    /// Count a string and decide its form. The writer's whole promotion policy lives
    /// here: `None` means "inline", `Some((block, id))` means "a code".
    pub fn settle(&mut self, text: &str) -> Option<(u8, u64)> {
        if let Some((block, id)) = self.promoted.get(text) {
            return Some((*block, *id));
        }
        let entry = self.counts.entry(text.to_string()).or_insert(0);
        *entry += 1;
        let count = *entry;
        let width = self.blocks.get(self.open)?.width;
        let threshold = threshold_for(text.len(), width, &self.tiers)?;
        if count < threshold {
            return None;
        }
        self.counts.remove(text);
        Some(self.promote(text))
    }

    /// Give a string a code, widening the chain when the open block is full.
    fn promote(&mut self, text: &str) -> (u8, u64) {
        if self.blocks[self.open].entries.len() >= self.block_entries as usize {
            self.close_block();
        }
        let block = self.open;
        let id = self.blocks[block].entries.len() as u64;
        self.blocks[block].entries.push(text.to_string());
        let code = code_of(block);
        self.promoted.insert(text.to_string(), (code, id));
        (code, id)
    }

    /// Write the open block to the dict file and start a wider one.
    fn close_block(&mut self) {
        if let Err(_error) = self.flush_open_block() {
            return;
        }
        let width = self.blocks[self.open]
            .width
            .checked_mul(2)
            .unwrap_or(self.blocks[self.open].width);
        self.blocks.push(Block {
            width,
            entries: Vec::new(),
            file_offset: 0,
        });
        self.open = self.blocks.len() - 1;
    }

    fn flush_open_block(&mut self) -> Result<(), Error> {
        let block = match self.blocks.get(self.open) {
            Some(block) if !block.entries.is_empty() => block.clone(),
            _ => return Ok(()),
        };
        let mut payload = Vec::new();
        for text in &block.entries {
            put_varint(&mut payload, text.len() as u64);
            payload.extend_from_slice(text.as_bytes());
        }
        let compressed = zstd::bulk::compress(&payload, self.level).map_err(|_| Error::Codec)?;
        let (codec, bytes) = if compressed.len() < payload.len() {
            (Codec::Zstd, compressed)
        } else {
            (Codec::Raw, payload.clone())
        };
        let mut header = [0u8; DICT_BLOCK_HEADER_LEN as usize];
        header[0..4].copy_from_slice(&DICT_BLOCK_MAGIC);
        header[4] = VERSION;
        header[5] = codec.code();
        header[6] = match block.width {
            2 => 0,
            4 => 1,
            _ => 2,
        };
        header[8..12].copy_from_slice(&(block.entries.len() as u32).to_be_bytes());
        let prev = if self.open == 0 {
            0
        } else {
            self.blocks[self.open - 1].file_offset
        };
        header[16..24].copy_from_slice(&prev.to_be_bytes());
        header[24..28].copy_from_slice(&(bytes.len() as u32).to_be_bytes());
        header[28..32].copy_from_slice(&(payload.len() as u32).to_be_bytes());

        let file = self.file.as_mut().ok_or(Error::BadPath)?;
        let at = self.file_offset;
        file.seek(SeekFrom::Start(at))?;
        file.write_all(&header)?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        self.file_offset = at + DICT_BLOCK_HEADER_LEN + bytes.len() as u64;
        self.blocks[self.open].file_offset = at;
        write_tail(&self.path, at)?;
        Ok(())
    }

    /// Close the dictionary: whatever is still in memory lands on disk. Called when
    /// the part is sealed.
    pub fn finish(&mut self) -> Result<(), Error> {
        self.flush_open_block()
    }

    /// Make the promoted strings durable before the records that refer to them are.
    /// A block that already carries a count cannot grow, so this closes it and opens a
    /// successor at the same width — the chain widens only when a block FILLS.
    pub fn flush_if_dirty(&mut self) -> Result<(), Error> {
        if self.blocks[self.open].entries.is_empty() {
            return Ok(());
        }
        self.flush_open_block()?;
        let width = self.blocks[self.open].width;
        self.blocks.push(Block {
            width,
            entries: Vec::new(),
            file_offset: 0,
        });
        self.open = self.blocks.len() - 1;
        Ok(())
    }
}

/// The count at which a string of `len` bytes earns a code, given the width of the
/// block it would land in. `None` = never (a string no longer than its own code only
/// grows by being interned).
pub fn threshold_for(len: usize, width: usize, tiers: &[(u32, u32)]) -> Option<u32> {
    if len <= width {
        return None;
    }
    for (multiple, threshold) in tiers {
        if *multiple == 0 {
            return Some(*threshold);
        }
        if len
            <= usize::try_from(*multiple)
                .unwrap_or(usize::MAX)
                .saturating_mul(width)
        {
            return Some(*threshold);
        }
    }
    None
}

struct DecodedBlock {
    width: usize,
    entries: Vec<String>,
}

fn read_block(file: &mut File, at: u64) -> Result<Option<(DecodedBlock, u64)>, Error> {
    file.seek(SeekFrom::Start(at))?;
    let mut header = [0u8; DICT_BLOCK_HEADER_LEN as usize];
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    if header[0..4] != DICT_BLOCK_MAGIC {
        return Ok(None);
    }
    let codec = Codec::from_code(header[5]).unwrap_or(Codec::Raw);
    let width = match width_of(header[6]) {
        Some(width) => width,
        None => return Ok(None),
    };
    let count = u32::from_be_bytes(match header[8..12].try_into() {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    });
    let len_c = u32::from_be_bytes(match header[24..28].try_into() {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    }) as usize;
    let len_u = u32::from_be_bytes(match header[28..32].try_into() {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    }) as usize;
    let mut bytes = vec![0u8; len_c];
    if file.read_exact(&mut bytes).is_err() {
        return Ok(None);
    }
    let payload = match codec {
        Codec::Raw => bytes,
        Codec::Zstd => match zstd::bulk::decompress(&bytes, len_u) {
            Ok(payload) => payload,
            Err(_) => return Ok(None),
        },
    };
    if payload.len() != len_u {
        return Ok(None);
    }
    let mut entries = Vec::with_capacity(count as usize);
    let mut pos = 0usize;
    while pos < payload.len() && entries.len() < count as usize {
        let len = match get_varint(&payload, &mut pos) {
            Some(len) => len as usize,
            None => break,
        };
        let end = match pos.checked_add(len) {
            Some(end) if end <= payload.len() => end,
            _ => break,
        };
        match std::str::from_utf8(&payload[pos..end]) {
            Ok(text) => entries.push(text.to_string()),
            Err(_) => break,
        }
        pos = end;
    }
    let next = at + DICT_BLOCK_HEADER_LEN + len_c as u64;
    Ok(Some((DecodedBlock { width, entries }, next)))
}

/// The newest block's offset, replaced by rename: growing the dictionary is an append
/// plus a rename, never a patch of a header.
fn write_tail(dict_path: &Path, offset: u64) -> Result<(), Error> {
    let tail = tail_path_for(dict_path)?;
    let temp = tail.with_extension("tail.tmp");
    {
        let mut file = File::create(&temp)?;
        file.write_all(&offset.to_be_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, &tail)?;
    Ok(())
}

pub fn tail_path_for(dict_path: &Path) -> Result<PathBuf, Error> {
    let stem = dict_path.file_stem().ok_or(Error::BadPath)?;
    let mut name = stem.to_os_string();
    name.push(".dict.tail");
    Ok(dict_path.with_file_name(name))
}

pub fn dict_path_for(part: &Path) -> Result<PathBuf, Error> {
    let stem = part.file_stem().ok_or(Error::BadPath)?;
    let mut name = stem.to_os_string();
    name.push(".dict");
    Ok(part.with_file_name(name))
}
