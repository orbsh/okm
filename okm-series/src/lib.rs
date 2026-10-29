//! An append-only log of timestamped records.
//!
//! The shape is ADR-0046: one directory per bucket (an hour by default), parts
//! inside it that are pure append and sealed by rename, varint deltas for time,
//! compressed blocks with an index beside them, and a body that is nTLV so a reader
//! can look without a schema.
//!
//! What this crate is NOT: a queue (no ack, no redelivery, no per-consumer cursor),
//! a table (no keys, faces, reduces or changelog), or a time-series database (no
//! aggregation). It is the append-and-read half, and the reason it can be a file
//! instead of a table is retention — a directory is dropped in one syscall, where an
//! LSM row leaves a tombstone behind.

pub mod body;
pub mod dict;
pub mod format;
pub mod part;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub use body::{Body, Value};
pub use dict::DictConfig;
pub use format::{Codec, Footer, IndexRow, PartHeader, Precision};

use part::{PartReader, PartWriter};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    BadFrame(&'static str),
    BadMeta(String),
    BadPath,
    Codec,
    Overflow,
    /// The log is monotone in time: a record older than its predecessor is refused,
    /// not clamped, because the byte order IS the time order.
    TimeBackwards {
        last: u64,
        given: u64,
    },
    Locked(u32),
    ConfigMismatch(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(error) => write!(f, "io: {error}"),
            Error::BadFrame(what) => write!(f, "malformed frame: {what}"),
            Error::BadMeta(what) => write!(f, "bad log.meta: {what}"),
            Error::BadPath => write!(f, "a part file has no stem"),
            Error::Codec => write!(f, "block codec failed"),
            Error::Overflow => write!(f, "a length or offset does not fit"),
            Error::TimeBackwards { last, given } => {
                write!(f, "time went backwards: last {last}, given {given}")
            }
            Error::Locked(pid) => write!(f, "the log is held by process {pid}"),
            Error::ConfigMismatch(what) => write!(f, "log.meta disagrees: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub precision: Precision,
    /// Seconds per bucket. One hour by default; buckets are aligned to absolute Unix
    /// seconds, so two logs on one machine agree on a directory name.
    pub bucket_seconds: u64,
    /// Records are compressed this many bytes at a time.
    pub block_bytes: usize,
    /// A part is sealed when it would grow past this, so retention has a unit.
    pub part_bytes: u64,
    pub zstd_level: i32,
    /// `Some` interns strings; `None` writes every string inline. The switch exists
    /// because block compression covers part of what interning was for (ADR-0046 §8),
    /// so the choice belongs to a measurement.
    pub dict: Option<crate::dict::DictConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            precision: Precision::Ns,
            bucket_seconds: 3600,
            block_bytes: 64 * 1024,
            part_bytes: 64 * 1024 * 1024,
            zstd_level: part::DEFAULT_ZSTD_LEVEL,
            dict: Some(crate::dict::DictConfig::default()),
        }
    }
}

/// What `log.meta` says: identity, written once, read by every process that opens the
/// log. Deliberately not where the index lives — appending block rows here would grow
/// identity without bound.
#[derive(Clone, Debug)]
pub struct Meta {
    pub epoch_unix_ms: u64,
    pub precision: Precision,
    pub bucket_seconds: u64,
}

struct Active {
    bucket: u64,
    writer: PartWriter,
}

pub struct Log {
    root: PathBuf,
    config: Config,
    meta: Meta,
    active: Option<Active>,
    /// The next logical offset a new part starts at.
    next_offset: u64,
}

impl Log {
    /// Open (or create) a log. The `LOCK` file names the writer; a stale pid is taken
    /// over, a live one is refused.
    pub fn open(root: &Path, config: Config) -> Result<Self, Error> {
        std::fs::create_dir_all(root)?;
        let meta_path = root.join("log.meta");
        let meta = if meta_path.exists() {
            let meta = read_meta(&meta_path)?;
            if meta.precision != config.precision {
                return Err(Error::ConfigMismatch(format!(
                    "precision is {}, not {}",
                    meta.precision.label(),
                    config.precision.label()
                )));
            }
            if meta.bucket_seconds != config.bucket_seconds {
                return Err(Error::ConfigMismatch(format!(
                    "bucket is {}s, not {}s",
                    meta.bucket_seconds, config.bucket_seconds
                )));
            }
            meta
        } else {
            let meta = Meta {
                epoch_unix_ms: now_unix_ms()?,
                precision: config.precision,
                bucket_seconds: config.bucket_seconds,
            };
            write_meta(&meta_path, &meta)?;
            meta
        };
        acquire_lock(root)?;

        let dict_config = config.dict.clone();
        let block_bytes = config.block_bytes;
        let zstd_level = config.zstd_level;
        let mut log = Log {
            root: root.to_path_buf(),
            config,
            meta,
            active: None,
            next_offset: 0,
        };

        // Recover: adopt the one open part if there is one, and learn where offsets
        // continue from the parts that are already there.
        for (bucket, path) in log.all_parts()? {
            let reader = PartReader::open(&path, dict_config.as_ref())?;
            let (_, end) = reader.offset_span();
            log.next_offset = log.next_offset.max(end);
            if !reader.sealed() && log.active.is_none() {
                let writer =
                    PartWriter::adopt(&path, block_bytes, zstd_level, dict_config.as_ref())?;
                log.next_offset = log.next_offset.max(writer.next_offset());
                log.active = Some(Active { bucket, writer });
            }
        }
        Ok(log)
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Append one body. Returns its logical offset, which is the only address this
    /// crate hands out: `read(offset)` is `read` starting there.
    ///
    /// The record is buffered until a block fills, so a reader sees it after the
    /// flush, not after the call (see `append_durable` for the caller that will not
    /// accept that window).
    pub fn append(&mut self, ts: u64, body: &Body) -> Result<u64, Error> {
        let bucket = self.bucket_of(ts);
        self.ensure_part(bucket, ts)?;
        let active = match self.active.as_mut() {
            Some(active) => active,
            None => return Err(Error::BadPath),
        };
        let offset = active.writer.append(ts, body)?;
        self.next_offset = self.next_offset.max(offset + 1);
        Ok(offset)
    }

    /// Append and make it durable before returning: the block holding this record is
    /// written and fsynced. Costs a block per record, which is the trade the caller
    /// chose.
    pub fn append_durable(&mut self, ts: u64, body: &Body) -> Result<u64, Error> {
        let offset = self.append(ts, body)?;
        if let Some(active) = self.active.as_mut() {
            active.writer.flush_block()?;
        }
        Ok(offset)
    }

    /// Write and fsync the buffered block without appending anything. This is the
    /// durability boundary a caller who batches wants: `append` a batch, then `flush`,
    /// then treat the batch as on disk.
    pub fn flush(&mut self) -> Result<(), Error> {
        match self.active.as_mut() {
            Some(active) => active.writer.flush_block(),
            None => Ok(()),
        }
    }

    /// Close the open part (footer, fsync, rename). The log stays usable: the next
    /// append opens a new part.
    pub fn seal(&mut self) -> Result<Option<Footer>, Error> {
        match self.active.take() {
            Some(active) => Ok(Some(active.writer.seal()?)),
            None => Ok(None),
        }
    }

    /// Read up to `max` records from a logical offset, across parts.
    pub fn read(&self, from: u64, max: usize) -> Result<Vec<(u64, u64, Body)>, Error> {
        let mut rows = Vec::new();
        for (_, path) in self.all_parts()? {
            if rows.len() >= max {
                break;
            }
            let reader = PartReader::open(&path, self.config.dict.as_ref())?;
            let (start, end) = reader.offset_span();
            if end <= from {
                continue;
            }
            // An offset that retention (or a lost tail) took away starts at the oldest
            // record the log still has: the caller sees the gap in the offsets.
            reader.read_from(from.max(start), max, &mut rows)?;
        }
        Ok(rows)
    }

    /// Read every record whose timestamp is in `[from_ts, to_ts]`, up to `max`.
    pub fn read_by_time(
        &self,
        from_ts: u64,
        to_ts: u64,
        max: usize,
    ) -> Result<Vec<(u64, u64, Body)>, Error> {
        let mut rows = Vec::new();
        for (_, path) in self.all_parts()? {
            if rows.len() >= max {
                break;
            }
            let reader = PartReader::open(&path, self.config.dict.as_ref())?;
            match reader.ts_span() {
                Some((first, last)) if last < from_ts || first > to_ts => continue,
                Some(_) => {}
                None => continue,
            }
            reader.read_by_time(from_ts, to_ts, max, &mut rows)?;
        }
        rows.sort_by_key(|(ts, offset, _)| (*ts, *offset));
        Ok(rows)
    }

    /// Drop every bucket older than the one `ts` falls in. This is the retention the
    /// crate exists for: one `remove_dir_all`, no tombstones.
    pub fn retain_before(&mut self, ts: u64) -> Result<Vec<u64>, Error> {
        let cutoff = self.bucket_of(ts);
        let mut dropped = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(name) => name,
                None => continue,
            };
            let bucket = match name.parse::<u64>() {
                Ok(bucket) => bucket,
                Err(_) => continue,
            };
            if bucket >= cutoff {
                continue;
            }
            if let Some(active) = self.active.as_ref()
                && active.bucket == bucket
            {
                continue;
            }
            std::fs::remove_dir_all(entry.path())?;
            dropped.push(bucket);
        }
        dropped.sort_unstable();
        Ok(dropped)
    }

    /// Every part on disk, oldest bucket first.
    pub fn parts(&self) -> Result<Vec<PartInfo>, Error> {
        let mut parts = Vec::new();
        for (bucket, path) in self.all_parts()? {
            let reader = PartReader::open(&path, self.config.dict.as_ref())?;
            let (first_offset, end) = reader.offset_span();
            let (first_ts, last_ts) = reader.ts_span().unwrap_or((0, 0));
            let bytes = std::fs::metadata(&path)?.len();
            parts.push(PartInfo {
                bucket,
                path,
                sealed: reader.sealed(),
                first_offset,
                records: end - first_offset,
                first_ts,
                last_ts,
                bytes,
            });
        }
        Ok(parts)
    }

    pub fn bucket_of(&self, ts: u64) -> u64 {
        let per_second = u128::from(self.meta.precision.per_second());
        let bucket = u128::from(self.meta.bucket_seconds);
        u64::try_from(u128::from(ts) / per_second / bucket).unwrap_or(u64::MAX)
    }

    fn bucket_dir(&self, bucket: u64) -> PathBuf {
        self.root.join(format!("{bucket:012}"))
    }

    fn ensure_part(&mut self, bucket: u64, ts: u64) -> Result<(), Error> {
        let needs_new = match self.active.as_ref() {
            None => true,
            Some(active) => {
                active.bucket != bucket
                    || active.writer.written_bytes() + active.writer.pending_bytes() as u64
                        >= self.config.part_bytes
            }
        };
        if !needs_new {
            return Ok(());
        }
        if self.active.is_some() {
            self.seal()?;
        }
        let dir = self.bucket_dir(bucket);
        std::fs::create_dir_all(&dir)?;
        let number = part::part_numbers(&dir)?.last().map(|n| n + 1).unwrap_or(0);
        for candidate in number..number.saturating_add(1024) {
            let path = part::open_path(&dir, candidate);
            if path.exists() {
                continue;
            }
            let header = PartHeader {
                precision: self.meta.precision,
                bucket,
                base_ts: ts,
                first_offset: self.next_offset,
                created_unix_ms: now_unix_ms()?,
            };
            let writer = PartWriter::create(
                &path,
                header,
                self.config.block_bytes,
                self.config.zstd_level,
                self.config.dict.as_ref(),
            )?;
            self.active = Some(Active { bucket, writer });
            return Ok(());
        }
        Err(Error::BadPath)
    }

    /// Every part in the log, as (bucket, path), oldest first.
    fn all_parts(&self) -> Result<Vec<(u64, PathBuf)>, Error> {
        let mut buckets: Vec<(u64, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(name) => name,
                None => continue,
            };
            if let Ok(bucket) = name.parse::<u64>() {
                buckets.push((bucket, entry.path()));
            }
        }
        buckets.sort_by_key(|(bucket, _)| *bucket);
        let mut parts = Vec::new();
        for (bucket, dir) in buckets {
            for path in part::sealed_parts(&dir)? {
                parts.push((bucket, path));
            }
            if let Some(path) = part::open_part(&dir)? {
                parts.push((bucket, path));
            }
        }
        Ok(parts)
    }
}

/// What `parts()` reports: the physical summary of a part, which is as far as the
/// statistics go (no field-level statistics — those belong to the lake).
#[derive(Clone, Debug)]
pub struct PartInfo {
    pub bucket: u64,
    pub path: PathBuf,
    pub sealed: bool,
    pub first_offset: u64,
    pub records: u64,
    pub first_ts: u64,
    pub last_ts: u64,
    pub bytes: u64,
}

fn read_meta(path: &Path) -> Result<Meta, Error> {
    let text = std::fs::read_to_string(path)?;
    let mut epoch_unix_ms = 0u64;
    let mut precision = Precision::Ns;
    let mut bucket_seconds = 3600u64;
    let mut seen = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        match key {
            "okm-series" => seen = true,
            "epoch" => {
                epoch_unix_ms = value
                    .parse()
                    .map_err(|_| Error::BadMeta("epoch".to_string()))?
            }
            "precision" => {
                precision = match value {
                    "s" => Precision::S,
                    "ms" => Precision::Ms,
                    "us" => Precision::Us,
                    "ns" => Precision::Ns,
                    other => return Err(Error::BadMeta(format!("precision {other}"))),
                }
            }
            "bucket" => {
                bucket_seconds = value
                    .parse()
                    .map_err(|_| Error::BadMeta("bucket".to_string()))?
            }
            _ => {}
        }
    }
    if !seen {
        return Err(Error::BadMeta("missing header".to_string()));
    }
    if bucket_seconds == 0 {
        return Err(Error::BadMeta("bucket of zero".to_string()));
    }
    Ok(Meta {
        epoch_unix_ms,
        precision,
        bucket_seconds,
    })
}

fn write_meta(path: &Path, meta: &Meta) -> Result<(), Error> {
    let text = format!(
        "okm-series {}\nepoch {}\nprecision {}\nbucket {}\n",
        format::VERSION,
        meta.epoch_unix_ms,
        meta.precision.label(),
        meta.bucket_seconds
    );
    let temp = path.with_extension("meta.tmp");
    {
        let mut file = File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)?;
    part::sync_dir(path.parent().unwrap_or(Path::new(".")))
}

fn acquire_lock(root: &Path) -> Result<(), Error> {
    let path = root.join("LOCK");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(pid) = text.trim().parse::<u32>()
        && pid != std::process::id()
        && Path::new(&format!("/proc/{pid}")).exists()
    {
        return Err(Error::Locked(pid));
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)?;
    file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn now_unix_ms() -> Result<u64, Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::BadMeta("clock before the epoch".to_string()))?;
    Ok(now.as_millis() as u64)
}
