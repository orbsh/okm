//! An append-only log of timestamped records.
//!
//! The shape is ADR-0046: one directory per bucket (an hour by default), parts inside
//! it that are pure append and sealed by rename, varint deltas for time, compressed
//! blocks with an index beside them, and a body that is nTLV so a reader can look
//! without a schema. A log may declare ONE partition field, whose value becomes a
//! directory between the bucket and the parts.
//!
//! What this crate is NOT: a queue (no ack, no redelivery, no per-consumer cursor), a
//! table (no keys, faces, reduces or changelog), or a time-series database (no
//! aggregation). It is the append-and-read half, and the reason it can be a file
//! instead of a table is retention — a directory is dropped in one syscall, where an
//! LSM row leaves a tombstone behind.

pub mod body;
pub mod dict;
pub mod format;
pub mod part;

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

pub use body::{Body, Value};
pub use dict::DictConfig;
pub use format::{Codec, Footer, IndexRow, PartHeader, Precision};

use part::{PartReader, PartWriter};

/// The prefix a partition directory carries. Hex, so any value that fits in a body can
/// be one, and the prefix separates it from a bucket directory (which is all digits).
const PARTITION_PREFIX: &str = "p-";

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
    /// A partitioned log got a record without the declared partition field. Refused
    /// rather than filed somewhere plausible: a layout that lies cannot be read back.
    MissingPartition(u16),
    Locked(u32),
    /// A second `Log` for the same root inside one process. The `LOCK` file names a pid,
    /// which cannot distinguish two writers that share one, and two writers over one part
    /// interleave their bytes into a record that neither of them wrote.
    AlreadyOpen(PathBuf),
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
            Error::MissingPartition(tag) => {
                write!(f, "the record has no partition field (tag {tag})")
            }
            Error::Locked(pid) => write!(f, "the log is held by process {pid}"),
            Error::AlreadyOpen(root) => write!(
                f,
                "the log at {} is already open in this process",
                root.display()
            ),
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

#[derive(Clone)]
pub struct Config {
    pub precision: Precision,
    /// Seconds per bucket. One hour by default; buckets are aligned to absolute Unix
    /// seconds, so two logs on one machine agree on a directory name.
    pub bucket_seconds: u64,
    /// Records are compressed this many bytes at a time. This is the write path's main
    /// knob: the per-record cost is dominated by the per-block work.
    pub block_bytes: usize,
    /// A part is sealed when it would grow past this, so retention has a unit.
    pub part_bytes: u64,
    pub zstd_level: i32,
    /// `Codec::Raw` writes blocks uncompressed. Compression is on by default because
    /// the measurement says it is nearly free (`docs/internals/benchmarks.md`). Per BLOCK,
    /// not per log: every block header records its codec, so a log reopened with another
    /// codec still reads the old blocks.
    pub block_codec: Codec,
    /// `Some` interns strings when WRITING; `None` writes every string inline. Reading does
    /// not depend on it — a part carries its dictionary beside it and the reader loads that
    /// file whether or not anyone asked for one, because a code in a block needs it to mean
    /// anything. This setting is a writer's tuning.
    pub dict: Option<DictConfig>,
    /// The body tag whose value partitions the log, declared once and never changed.
    /// `None` = every part lives directly under its bucket.
    pub partition_tag: Option<u16>,
    /// The log's notion of "now". `None` = the system clock, which is what production
    /// uses; tests set it to make buckets and windows deterministic.
    ///
    /// It is NOT a way for a caller to stamp a record: `append` takes no timestamp at
    /// all, because the write time is the log's to decide (ADR-0046 §2).
    pub clock: Option<Clock>,
}

/// A source of timestamps in the log's precision. Tests use it; production leaves
/// `Config::clock` empty.
pub type Clock = std::sync::Arc<dyn Fn() -> u64 + Send + Sync>;

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("precision", &self.precision)
            .field("bucket_seconds", &self.bucket_seconds)
            .field("block_bytes", &self.block_bytes)
            .field("part_bytes", &self.part_bytes)
            .field("zstd_level", &self.zstd_level)
            .field("block_codec", &self.block_codec)
            .field("dict", &self.dict)
            .field("partition_tag", &self.partition_tag)
            .field("clock", &self.clock.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            precision: Precision::Ns,
            bucket_seconds: 3600,
            block_bytes: 64 * 1024,
            part_bytes: 64 * 1024 * 1024,
            zstd_level: part::DEFAULT_ZSTD_LEVEL,
            block_codec: Codec::Zstd,
            dict: Some(DictConfig::default()),
            partition_tag: None,
            clock: None,
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
    pub partition_tag: Option<u16>,
}

struct Active {
    bucket: u64,
    partition: Option<String>,
    writer: PartWriter,
}

pub struct Log {
    root: PathBuf,
    /// The in-process claim on this root. Held for its `Drop` — dropping the log (or an
    /// open that fails after the claim) releases it, so reopening after a close works.
    _claim: RootClaim,
    config: Config,
    meta: Meta,
    /// One writer per (bucket, partition): a partition is a file's worth of writer, and
    /// the single-writer rule is per file.
    active: Vec<Active>,
    /// The newest timestamp each stream has seen. Monotone time is a property of the
    /// STREAM, not of a part: without this, a record an hour older than its predecessor
    /// would land in an older bucket, open a fresh part with no history, and pass.
    last_ts: HashMap<Option<String>, u64>,
}

/// The identity a caller can act on. The writers, the per-stream clock and the held claim
/// are left out on purpose — none of them is something a log's owner can do anything with,
/// and `finish_non_exhaustive` says so rather than pretending the list is complete.
impl std::fmt::Debug for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Log")
            .field("root", &self.root)
            .field("precision", &self.meta.precision)
            .field("bucket_seconds", &self.meta.bucket_seconds)
            .field("partition_tag", &self.meta.partition_tag)
            .finish_non_exhaustive()
    }
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
            if meta.partition_tag != config.partition_tag {
                return Err(Error::ConfigMismatch(format!(
                    "partition tag is {:?}, not {:?}",
                    meta.partition_tag, config.partition_tag
                )));
            }
            meta
        } else {
            let meta = Meta {
                epoch_unix_ms: now_unix_ms()?,
                precision: config.precision,
                bucket_seconds: config.bucket_seconds,
                partition_tag: config.partition_tag,
            };
            write_meta(&meta_path, &meta)?;
            meta
        };
        acquire_lock(root)?;
        // The LOCK file is a pid, so it stops another process and says nothing about a
        // second `Log` here. Two writers over one part interleave their bytes into a
        // record neither of them wrote, so the claim is taken before any writer exists.
        let claim = RootClaim::claim(std::fs::canonicalize(root)?)?;

        let dict_config = config.dict.clone();
        let block_bytes = config.block_bytes;
        let zstd_level = config.zstd_level;
        let block_codec = config.block_codec;
        let mut log = Log {
            root: root.to_path_buf(),
            _claim: claim,
            config,
            meta,
            active: Vec::new(),
            last_ts: HashMap::new(),
        };

        // Recover: adopt the open parts (at most one per partition), and learn where
        // offsets continue from everything that is already there.
        for (bucket, partition, path) in log.all_parts()? {
            let reader = PartReader::open(&path, dict_config.as_ref())?;
            if let Some((_, last)) = reader.ts_span() {
                let seen = log.last_ts.entry(partition.clone()).or_insert(last);
                *seen = (*seen).max(last);
            }
            if reader.sealed() || log.active_part(bucket, partition.as_deref()).is_some() {
                continue;
            }
            let writer = PartWriter::adopt(
                &path,
                block_bytes,
                zstd_level,
                block_codec,
                dict_config.as_ref(),
            )?;
            // A part in a bucket the clock has left is finished: no record can land in it
            // (a record carries `max(clock, last)`), and an unsealed part has no footer and
            // keeps its bucket from retention. Sealing it here is the restart half of the
            // idle-log answer; the running half is `retain_before`.
            if bucket < log.bucket_of(log.raw_now()) {
                writer.seal()?;
                continue;
            }
            log.active.push(Active {
                bucket,
                partition,
                writer,
            });
        }
        Ok(log)
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Append one body at the write time. Returns its logical offset, which is the only
    /// address this crate hands out: `read(offset)` is `read` starting there.
    ///
    /// **The caller does not supply the time** (ADR-0046 §2). There is no timestamp
    /// argument: the log stamps the record when it is written, so the time axis is
    /// ingestion order and cannot contradict itself. A body may of course carry the
    /// application's own time under any tag it likes — that value is data, not the
    /// record's address.
    ///
    /// The record is buffered until a block fills, so a reader sees it after the flush,
    /// not after the call (see `append_durable` and `flush` for the callers who will not
    /// accept that window).
    pub fn append(&mut self, body: &Body) -> Result<u64, Error> {
        let partition = match self.meta.partition_tag {
            Some(tag) => Some(partition_of(body, tag)?),
            None => None,
        };
        let ts = self.now(&partition);
        let bucket = self.bucket_of(ts);
        self.ensure_part(bucket, &partition, ts)?;
        let index = self
            .active_index(bucket, partition.as_deref())
            .ok_or(Error::BadPath)?;
        let offset = self.active[index].writer.append(ts, body)?;
        self.last_ts.insert(partition, ts);
        Ok(offset)
    }

    /// Append and make it durable before returning: the block holding this record is
    /// written and fsynced. Costs a block per record, which is the trade the caller
    /// chose.
    pub fn append_durable(&mut self, body: &Body) -> Result<u64, Error> {
        let offset = self.append(body)?;
        self.flush()?;
        Ok(offset)
    }

    /// Write and fsync every buffered block without appending anything. This is the
    /// durability boundary a caller who batches wants: `append` a batch, then `flush`,
    /// then treat the batch as on disk.
    pub fn flush(&mut self) -> Result<(), Error> {
        for active in self.active.iter_mut() {
            active.writer.flush_block()?;
        }
        Ok(())
    }

    /// Close every open part (footer, fsync, rename). The log stays usable: the next
    /// append opens new parts.
    pub fn seal(&mut self) -> Result<Vec<Footer>, Error> {
        let mut footers = Vec::with_capacity(self.active.len());
        for active in std::mem::take(&mut self.active) {
            footers.push(active.writer.seal()?);
        }
        Ok(footers)
    }

    /// Read up to `max` records from a logical offset, across parts and partitions.
    ///
    /// With partitions the offset applies to EVERY stream (offsets are per stream, see
    /// ADR-0046 9) and rows come back in time order.
    pub fn read(&self, from: u64, max: usize) -> Result<Vec<(u64, u64, Body)>, Error> {
        let mut rows = Vec::new();
        for (_, _, path) in self.parts_by_offset()? {
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
        // Across partitions the parts arrive in directory order, so the caller gets a
        // time order it can act on rather than a directory order it did not ask for.
        rows.sort_by_key(|(ts, offset, _)| (*ts, *offset));
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
        for (_, _, path) in self.parts_by_offset()? {
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

    /// Drop every bucket older than the one `ts` falls in — every partition inside it,
    /// because a bucket is a time interval and a partition is not.
    pub fn retain_before(&mut self, ts: u64) -> Result<Vec<u64>, Error> {
        let cutoff = self.bucket_of(ts);
        // A part older than the cutoff will not be appended to again, so sealing it here is
        // what lets the drop below happen at all: otherwise "retention never touches a
        // bucket with a live writer" keeps last night's bucket alive until some later append
        // happens to cross a boundary — which for an idle log is never. ADR-0046 §3 has no
        // timer, so the caller's own call is the tick.
        self.seal_parts_older_than(cutoff, None)?;
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
            if bucket >= cutoff || self.active.iter().any(|active| active.bucket == bucket) {
                continue;
            }
            std::fs::remove_dir_all(entry.path())?;
            dropped.push(bucket);
        }
        dropped.sort_unstable();
        Ok(dropped)
    }

    /// Every part on disk, ordered by bucket and then by the offset it starts at.
    pub fn parts(&self) -> Result<Vec<PartInfo>, Error> {
        let mut parts = Vec::new();
        for (bucket, partition, path) in self.parts_by_offset()? {
            let reader = PartReader::open(&path, self.config.dict.as_ref())?;
            let (first_offset, end) = reader.offset_span();
            let (first_ts, last_ts) = reader.ts_span().unwrap_or((0, 0));
            let bytes = std::fs::metadata(&path)?.len();
            parts.push(PartInfo {
                bucket,
                partition,
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

    /// The write time for the next record of this stream, in the log's precision.
    ///
    /// A system clock can step backwards (NTP, a suspended machine). That is not the
    /// caller's error and must not become a failed write, so the timestamp is clamped to
    /// the stream's last one: two records may share a timestamp, and their offsets still
    /// order them.
    fn now(&self, partition: &Option<String>) -> u64 {
        let raw = self.raw_now();
        match self.last_ts.get(partition) {
            Some(last) => raw.max(*last),
            None => raw,
        }
    }

    /// The clock, unclamped. Sealing decisions ask "which bucket does the clock say we are
    /// in", not "what time may the next record carry".
    fn raw_now(&self) -> u64 {
        match self.config.clock.as_ref() {
            Some(clock) => clock(),
            None => unix_now(self.meta.precision),
        }
    }

    pub fn bucket_of(&self, ts: u64) -> u64 {
        let per_second = u128::from(self.meta.precision.per_second());
        let bucket = u128::from(self.meta.bucket_seconds);
        u64::try_from(u128::from(ts) / per_second / bucket).unwrap_or(u64::MAX)
    }

    fn bucket_dir(&self, bucket: u64) -> PathBuf {
        self.root.join(format!("{bucket:012}"))
    }

    fn partition_dir(&self, bucket: u64, partition: Option<&str>) -> PathBuf {
        let dir = self.bucket_dir(bucket);
        match partition {
            Some(value) => dir.join(format!("{PARTITION_PREFIX}{}", hex(value.as_bytes()))),
            None => dir,
        }
    }

    fn active_index(&self, bucket: u64, partition: Option<&str>) -> Option<usize> {
        self.active
            .iter()
            .position(|active| active.bucket == bucket && active.partition.as_deref() == partition)
    }

    fn active_part(&self, bucket: u64, partition: Option<&str>) -> Option<&PartWriter> {
        self.active_index(bucket, partition)
            .map(|index| &self.active[index].writer)
    }

    /// Seal every active part whose bucket is older than `cutoff`, optionally limited to one
    /// partition. `ensure_part` calls it for the stream it is writing (a bucket crossing);
    /// `retain_before` calls it with no filter, because a bucket it is about to drop must not
    /// be held open by a writer that will never append there again.
    fn seal_parts_older_than(
        &mut self,
        cutoff: u64,
        only: Option<&Option<String>>,
    ) -> Result<(), Error> {
        let stale: Vec<usize> = self
            .active
            .iter()
            .enumerate()
            .filter(|(_, active)| active.bucket < cutoff)
            .filter(|(_, active)| only.is_none_or(|partition| active.partition == *partition))
            .map(|(index, _)| index)
            .collect();
        for index in stale.into_iter().rev() {
            let active = self.active.remove(index);
            active.writer.seal()?;
        }
        Ok(())
    }

    fn ensure_part(
        &mut self,
        bucket: u64,
        partition: &Option<String>,
        ts: u64,
    ) -> Result<(), Error> {
        // Time has moved into a later bucket: the previous one's part is finished. Left
        // open it would keep its bucket permanent, because retention refuses to drop a
        // bucket that still has a writer.
        self.seal_parts_older_than(bucket, Some(partition))?;
        let full = match self.active_index(bucket, partition.as_deref()) {
            None => true,
            Some(index) => {
                let writer = &self.active[index].writer;
                writer.written_bytes() + writer.pending_bytes() as u64 >= self.config.part_bytes
            }
        };
        if !full {
            return Ok(());
        }
        if let Some(index) = self.active_index(bucket, partition.as_deref()) {
            let active = self.active.remove(index);
            active.writer.seal()?;
        }
        let dir = self.partition_dir(bucket, partition.as_deref());
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
                first_offset: self.stream_next_offset(bucket, partition.as_deref())?,
                created_unix_ms: now_unix_ms()?,
            };
            let writer = PartWriter::create(
                &path,
                header,
                self.config.block_bytes,
                self.config.zstd_level,
                self.config.block_codec,
                self.config.dict.as_ref(),
            )?;
            self.active.push(Active {
                bucket,
                partition: partition.clone(),
                writer,
            });
            return Ok(());
        }
        Err(Error::BadPath)
    }

    /// Where the next part of one stream starts: the highest offset any part of that
    /// (bucket, partition) reached. Offsets are per stream, because a block header holds
    /// only the first one and the reader counts up from there.
    fn stream_next_offset(&self, bucket: u64, partition: Option<&str>) -> Result<u64, Error> {
        let dir = self.partition_dir(bucket, partition);
        let mut paths = part::sealed_parts(&dir)?;
        if let Some(open) = part::open_part(&dir)? {
            paths.push(open);
        }
        let mut end = 0u64;
        for path in paths {
            if let Ok(reader) = PartReader::open(&path, None) {
                let (_, high) = reader.offset_span();
                end = end.max(high);
            }
        }
        Ok(end)
    }

    /// Every part, ordered by bucket, then partition, then part number.
    fn all_parts(&self) -> Result<Vec<(u64, Option<String>, PathBuf)>, Error> {
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
                parts.push((bucket, None, path));
            }
            if let Some(path) = part::open_part(&dir)? {
                parts.push((bucket, None, path));
            }
            let mut partitions: Vec<(String, PathBuf)> = Vec::new();
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let name = match name.to_str() {
                    Some(name) => name,
                    None => continue,
                };
                if let Some(value) = partition_of_dir(name) {
                    partitions.push((value, entry.path()));
                }
            }
            partitions.sort_by(|left, right| left.0.cmp(&right.0));
            for (value, partition_dir) in partitions {
                for path in part::sealed_parts(&partition_dir)? {
                    parts.push((bucket, Some(value.clone()), path));
                }
                if let Some(path) = part::open_part(&partition_dir)? {
                    parts.push((bucket, Some(value), path));
                }
            }
        }
        Ok(parts)
    }

    /// The same parts, ordered by the logical offset they start at — the order a reader
    /// has to walk them in once offsets are global and partitions interleave.
    fn parts_by_offset(&self) -> Result<Vec<(u64, Option<String>, PathBuf)>, Error> {
        let parts = self.all_parts()?;
        let mut keyed = Vec::with_capacity(parts.len());
        for part in parts {
            // The header answers the ordering question; opening the part would parse an
            // index that `read` is about to parse again.
            let first_offset = match part::first_offset_of(&part.2) {
                Ok(first_offset) => first_offset,
                Err(_) => continue,
            };
            keyed.push((first_offset, part));
        }
        keyed.sort_by_key(|(first_offset, _)| *first_offset);
        Ok(keyed.into_iter().map(|(_, part)| part).collect())
    }
}

/// What `parts()` reports: the physical summary of a part, which is as far as the
/// statistics go (no field-level statistics — those belong to the lake).
#[derive(Clone, Debug)]
pub struct PartInfo {
    pub bucket: u64,
    /// The partition value this part lives under, decoded from the directory name.
    pub partition: Option<String>,
    pub path: PathBuf,
    pub sealed: bool,
    pub first_offset: u64,
    pub records: u64,
    pub first_ts: u64,
    pub last_ts: u64,
    pub bytes: u64,
}

/// The value of the declared partition tag, rendered for a directory name. A record
/// without it is refused: filing it somewhere plausible would make the layout lie.
fn partition_of(body: &Body, tag: u16) -> Result<String, Error> {
    let value = body.get(tag).ok_or(Error::MissingPartition(tag))?;
    Ok(match value {
        Value::Str(text) => text.clone(),
        Value::U64(number) => number.to_string(),
        Value::I64(number) => number.to_string(),
        Value::F64(number) => number.to_string(),
        Value::Bytes(bytes) => hex(bytes),
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    Some(out)
}

fn partition_of_dir(name: &str) -> Option<String> {
    let rest = name.strip_prefix(PARTITION_PREFIX)?;
    String::from_utf8(unhex(rest)?).ok()
}

fn read_meta(path: &Path) -> Result<Meta, Error> {
    let text = std::fs::read_to_string(path)?;
    let mut epoch_unix_ms = 0u64;
    let mut precision = Precision::Ns;
    let mut bucket_seconds = 3600u64;
    let mut partition_tag = None;
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
            "partition" if value != "-" => {
                partition_tag = Some(
                    value
                        .parse()
                        .map_err(|_| Error::BadMeta("partition".to_string()))?,
                );
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
        partition_tag,
    })
}

fn write_meta(path: &Path, meta: &Meta) -> Result<(), Error> {
    let partition = match meta.partition_tag {
        Some(tag) => tag.to_string(),
        None => "-".to_string(),
    };
    let text = format!(
        "okm-series {}\nepoch {}\nprecision {}\nbucket {}\npartition {}\n",
        format::VERSION,
        meta.epoch_unix_ms,
        meta.precision.label(),
        meta.bucket_seconds,
        partition
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

/// The roots this process holds open, keyed by canonical path.
///
/// `LOCK` names a pid, so it enforces one writer per PROCESS, not one writer per root: a
/// second `Log` for the same directory inside this process passes the pid check and
/// interleaves its appends into the same part. That hole is what the registry closes, and
/// it is why the key is canonical — `root`, `root/.`, and a symlink to `root` are one root.
fn open_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static OPEN_ROOTS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    OPEN_ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// A claimed root, released when the log that holds it goes away — including the log of an
/// open that fails after the claim, so a failed `Log::open` never wedges the directory.
struct RootClaim {
    root: PathBuf,
}

impl RootClaim {
    fn claim(root: PathBuf) -> Result<Self, Error> {
        // A poisoned registry means a holder panicked while the lock was taken; the set is
        // still exactly as consistent as it was, so the entries are kept rather than
        // thrown away and handed to a second writer.
        let mut held = open_roots().lock().unwrap_or_else(PoisonError::into_inner);
        if !held.insert(root.clone()) {
            return Err(Error::AlreadyOpen(root));
        }
        Ok(RootClaim { root })
    }
}

impl Drop for RootClaim {
    fn drop(&mut self) {
        open_roots()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.root);
    }
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

/// Wall clock in the log's precision. Seconds/millis/micros are exact; nanos come from
/// the nanosecond field, which is the whole point of declaring a precision at all.
fn unix_now(precision: Precision) -> u64 {
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) => now,
        Err(_) => return 0,
    };
    match precision {
        Precision::S => now.as_secs(),
        Precision::Ms => now.as_millis() as u64,
        Precision::Us => now.as_micros() as u64,
        Precision::Ns => now.as_nanos() as u64,
    }
}

fn now_unix_ms() -> Result<u64, Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::BadMeta("clock before the epoch".to_string()))?;
    Ok(now.as_millis() as u64)
}
