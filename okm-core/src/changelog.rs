//! The change log (ADR-0045): capture at the commit boundary, drain out of
//! process.
//!
//! A collection built with [`crate::Collection::with_changelog`] appends ONE
//! frame per committed batch to a segment file:
//!
//! ```text
//! <root>/<header hex>/<nnnnnnnn>.partial   ← being written
//! <root>/<header hex>/<nnnnnnnn>.segment   ← sealed: immutable, drained
//! <root>/<header hex>/schema-v<N>.json     ← sidecar (written by the host)
//! ```
//!
//! The frame is [`crate::OpFrame`]'s op encoding behind a 4-byte big-endian
//! length, so a reader needs `okm-wire` and nothing else. It is NOT a second
//! format: the same codec the remote backend ships (ADR-0010).
//!
//! **What is captured is the TRUTH half of the batch** — primary rows (slot
//! `0x00`), the dynamic segment (`0x01`), the two dynamic field-name faces
//! (`0x02`/`0x03`), the two predicate faces (`0x04`/`0x05`), and both directions
//! of every declared categorical vocabulary (families `0x1`/`0x2`, ADR-0032:
//! the payload stores ids, so the labels are truth, not decoration). The DERIVED
//! half is dropped — declared indexes (`0x3nnn`), reduces (`0x4nnn`), junctions
//! (`0x5nnn`), the graph's traversal faces (`0x06`–`0x09`, rebuilt from the
//! primary rows by `Graph::reindex`), the vector snapshot (`0x0A`) and the table
//! meta row (`0x0F`) — as is any bare-face key (a face's singleton, e.g. a
//! vocabulary's allocation cursor).
//!
//! The rule FAILS CLOSED: an unrecognised slot is dropped. A log that is visibly
//! incomplete can be repaired by a reconciliation scan; a lake that quietly
//! holds derived state cannot. Adding a face is a code change (ADR-0030), and
//! updating this filter is part of it — nothing checks that for you.
//!
//! Guarantees, and what each one costs:
//!
//! - **never a phantom row** — nothing is appended until the engine commit
//!   returned `Ok` (the bytes are staged before it, buffered, and written after
//!   it), so a record for a refused write never reaches the file;
//! - **at most once** — a crash between the commit and the append loses the
//!   tail; the reconciliation scan is the backstop (ADR-0045 §6);
//! - **one memcpy per write, no fsync** — records go into a `buffer_bytes`
//!   buffer, which is flushed when full or on rotation; `sync_data` opts into a
//!   per-flush `fsync` and changes nothing else;
//! - **no producer counter** — the address is `(segment, offset)`: the next
//!   segment number is the highest name in the directory plus one, so there is
//!   no sequence to reserve and no hole to reason about.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::OpFrame;
use crate::error::{Error, Result};
use crate::model::index::{
    DYNAMIC_SLOT, FIELD_VOCAB_ID_SLOT, FIELD_VOCAB_NAME_SLOT, FIXED_FACE_MAX,
    PREDICATE_VOCAB_ID_SLOT, PREDICATE_VOCAB_NAME_SLOT, PRIMARY_SLOT, Slot, segment,
};

/// Extension of a segment still being written. Readers ignore it: its tail may
/// be torn (ADR-0045 §5).
pub const PARTIAL_EXT: &str = "partial";
/// Extension of a sealed segment: immutable, ordered by name, drainable.
pub const SEALED_EXT: &str = "segment";

/// How much of the change log to keep in flight (ADR-0045 §12).
///
/// The first three knobs are the writer's rotation policy. The export window is
/// the DRAINER's policy and is deliberately not configurable here: one segment
/// is a rotation unit, not a file the sink has to accept.
#[derive(Debug, Clone)]
pub struct ChangelogConfig {
    /// Root directory. Each table header gets its own subdirectory, so streams
    /// cannot collide.
    pub dir: PathBuf,
    /// Seal the current segment once it reaches this many bytes.
    pub segment_bytes: u64,
    /// Seal the current segment once it is this old (bytes first, then age).
    pub segment_seconds: u64,
    /// Buffer size before a flush: the whole per-write cost is one memcpy into
    /// this buffer, so it trades memory for syscalls.
    pub buffer_bytes: usize,
    /// `false` (default) is the at-most-once contract with no per-write fsync.
    /// `true` narrows the crash window to one flush and changes nothing else —
    /// it never creates a phantom row either way.
    pub sync_data: bool,
}

impl ChangelogConfig {
    pub const DEFAULT_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
    pub const DEFAULT_SEGMENT_SECONDS: u64 = 300;
    pub const DEFAULT_BUFFER_BYTES: usize = 1024 * 1024;

    /// The defaults the ADR records; only the directory is required.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            segment_bytes: Self::DEFAULT_SEGMENT_BYTES,
            segment_seconds: Self::DEFAULT_SEGMENT_SECONDS,
            buffer_bytes: Self::DEFAULT_BUFFER_BYTES,
            sync_data: false,
        }
    }
}

/// What the log owes, for a host that wants to watch it: pending bytes, records
/// discarded because the sink stayed broken, and the last write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogStatus {
    pub segment: u64,
    pub segment_written: u64,
    pub pending_bytes: usize,
    pub records_dropped: u64,
    pub last_error: Option<String>,
}

/// The writer. One per collection, driven only by committed writes: there is no
/// timer and no background thread (ADR-0045 §7).
pub struct Changelog {
    dir: PathBuf,
    header: Vec<u8>,
    cfg: ChangelogConfig,
    /// `None` while sealed: the rename happens with the handle closed, and a
    /// failed flush leaves this `Some` so the next attempt retries.
    file: Option<File>,
    segment: u64,
    written: u64,
    opened: Instant,
    /// Bytes for the file that have not been handed to it yet.
    pending: Vec<u8>,
    /// One commit's records, built before the commit and moved into `pending`
    /// after it.
    staged: Vec<u8>,
    dropped: u64,
    last_error: Option<String>,
}

impl Changelog {
    /// Open the table's directory and start the next segment.
    ///
    /// An existing `.partial` is LEFT ALONE, never appended to: its tail may be
    /// torn, and a writer cannot tell where the last good frame ends without
    /// reading it (which the drainer does). Starting a fresh segment number is
    /// what keeps a crash from corrupting a stream that already has readers.
    pub fn open(cfg: ChangelogConfig, header: &[u8]) -> Result<Self> {
        let dir = table_dir(&cfg.dir, header);
        std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        let segment = next_segment(&dir)?;
        let file = create_segment(&dir, segment)?;
        Ok(Self {
            dir,
            header: header.to_vec(),
            cfg,
            file: Some(file),
            segment,
            written: 0,
            opened: Instant::now(),
            pending: Vec::new(),
            staged: Vec::new(),
            dropped: 0,
            last_error: None,
        })
    }

    /// Build one commit's frame into the staging buffer. Infallible on purpose:
    /// it only fills memory, and the write path must not fail because a log
    /// exists (ADR-0045 §6 — the log is a hint, the store is the record).
    ///
    /// An empty carried set stages nothing: a batch made only of derived writes
    /// (an index rebuild, a reduce settle) is not a change to the truth.
    /// Returns how many ops it carried: the staged/flush split (`stage_capture`
    /// / `flush_capture`) has to tell "nothing to say" from "say it later".
    pub fn stage(&mut self, ops: &[(Vec<u8>, Option<Vec<u8>>)]) -> usize {
        self.staged.clear();
        let count = ops
            .iter()
            .filter(|(key, _)| carried(&self.header, key))
            .count();
        if count == 0 {
            return 0;
        }
        let start = self.staged.len();
        // The outer length prefix: a file is a byte stream, and the frame codec
        // deliberately carries no total-length header (okm-wire, reuse
        // contract). Backfilled once the body is encoded.
        self.staged.extend_from_slice(&[0, 0, 0, 0]);
        okm_wire::put_len(&mut self.staged, count);
        for (key, value) in ops {
            if !carried(&self.header, key) {
                continue;
            }
            match value {
                Some(value) => {
                    okm_wire::put_op_header(
                        &mut self.staged,
                        okm_wire::OP_PUT,
                        key.len(),
                        value.len(),
                    );
                    self.staged.extend_from_slice(key);
                    self.staged.extend_from_slice(value);
                }
                None => {
                    okm_wire::put_op_header(&mut self.staged, okm_wire::OP_DELETE, key.len(), 0);
                    self.staged.extend_from_slice(key);
                }
            }
        }
        let body = self.staged.len().saturating_sub(start + 4);
        let Ok(len) = u32::try_from(body) else {
            // A single batch larger than 4 GiB is not representable in the
            // frame's own count field either; drop the record rather than write
            // a length the reader would misinterpret.
            self.staged.truncate(start);
            return 0;
        };
        if let Some(prefix) = self.staged.get_mut(start..start + 4) {
            prefix.copy_from_slice(&len.to_be_bytes());
        }
        count
    }

    /// Move the staged frame into the file buffer and flush/rotate if due.
    ///
    /// Fallible because the file is: a failure KEEPS the bytes (the next call
    /// retries) and records itself in [`Changelog::status`]. The caller decides
    /// whether that is an error — the collection treats it as a status, because
    /// the write it describes is already committed.
    pub fn commit_staged(&mut self) -> Result<()> {
        if self.staged.is_empty() {
            return Ok(());
        }
        self.pending.append(&mut self.staged);
        // The buffer is a syscall-batching knob; the segment size is not. A
        // record that would cross `segment_bytes` flushes even when the buffer
        // is far from full, or a small-buffer config would never rotate.
        let buffered = self.written.saturating_add(self.pending.len() as u64);
        if self.pending.len() >= self.cfg.buffer_bytes || buffered >= self.cfg.segment_bytes {
            self.write_pending()?;
        }
        self.maybe_rotate()
    }

    /// Seal the current segment now — the caller's rotation, for a host that
    /// wants a window boundary at a moment of its choosing.
    pub fn seal(&mut self) -> Result<()> {
        self.write_pending()?;
        self.file = None;
        std::fs::rename(self.partial_path(), self.sealed_path())
            .map_err(|e| io_err(&self.partial_path(), e))?;
        self.segment += 1;
        self.written = 0;
        self.opened = Instant::now();
        self.file = Some(create_segment(&self.dir, self.segment)?);
        Ok(())
    }

    pub fn status(&self) -> ChangelogStatus {
        ChangelogStatus {
            segment: self.segment,
            segment_written: self.written,
            pending_bytes: self.pending.len(),
            records_dropped: self.dropped,
            last_error: self.last_error.clone(),
        }
    }

    /// The table's log directory, so a host can write the schema sidecar beside
    /// the segments without re-deriving the path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn partial_path(&self) -> PathBuf {
        self.dir
            .join(format!("{:08}.{}", self.segment, PARTIAL_EXT))
    }

    fn sealed_path(&self) -> PathBuf {
        self.dir.join(format!("{:08}.{}", self.segment, SEALED_EXT))
    }

    fn write_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let path = self.partial_path();
        let Some(file) = self.file.as_mut() else {
            self.last_error = Some(format!("{}: segment handle is closed", path.display()));
            return Err(Error::Changelog {
                what: format!("{}: segment handle is closed", path.display()),
            });
        };
        if let Err(e) = file.write_all(&self.pending) {
            // Keep the bytes: the next flush retries. Memory stays bounded by
            // `trim_to_cap`, which drops the OLDEST records — the window they
            // describe is recoverable by a reconciliation scan.
            let what = format!("{}: {e}", path.display());
            self.last_error = Some(what.clone());
            self.dropped += self.trim_to_cap();
            return Err(Error::Changelog { what });
        }
        self.written += self.pending.len() as u64;
        self.pending.clear();
        if self.cfg.sync_data
            && let Err(e) = file.sync_data()
        {
            let what = format!("{}: {e}", path.display());
            self.last_error = Some(what.clone());
            return Err(Error::Changelog { what });
        }
        Ok(())
    }

    /// Bound the retry buffer: a broken sink must not grow the process without
    /// limit. Drops whole records from the front and counts them.
    fn trim_to_cap(&mut self) -> u64 {
        let cap = self.cfg.buffer_bytes.saturating_mul(8).max(1);
        let mut dropped = 0u64;
        while self.pending.len() > cap {
            let Some(end) = first_record_end(&self.pending) else {
                // No complete record: drop the unreadable prefix.
                self.pending.clear();
                return dropped + 1;
            };
            self.pending.drain(..end);
            dropped += 1;
        }
        dropped
    }

    fn maybe_rotate(&mut self) -> Result<()> {
        let total = self.written.saturating_add(self.pending.len() as u64);
        let by_bytes = total >= self.cfg.segment_bytes;
        let by_age = self.opened.elapsed().as_secs() >= self.cfg.segment_seconds;
        // An empty segment never rotates: age alone would leave a trail of empty
        // files behind a process that writes once a day.
        if !(by_bytes || by_age) || total == 0 {
            return Ok(());
        }
        self.seal()
    }
}

impl Drop for Changelog {
    /// A clean shutdown seals what it wrote: a sealed segment is what a drainer
    /// can consume, and leaving the last window unsealed would strand it. A
    /// failed flush is not retried here (nothing can report it) — the remaining
    /// `.partial` is ignored by readers, which is exactly the crash contract.
    fn drop(&mut self) {
        let _ = self.write_pending();
        self.file = None;
        if self.written > 0 {
            let _ = std::fs::rename(self.partial_path(), self.sealed_path());
        }
    }
}

/// The directory a table header owns under the changelog root.
pub fn table_dir(root: &Path, header: &[u8]) -> PathBuf {
    root.join(hex(header))
}

/// Name of the schema sidecar for one layout version. One file per version, not
/// per segment: the declaration is a property of the shape, and a shape change
/// changes the number (ADR-0042).
#[cfg(feature = "schema-serde")]
pub fn sidecar_name(layout_version: u8) -> String {
    format!("schema-v{layout_version}.json")
}

/// Write the table's declaration beside its segments, ONCE per layout version.
///
/// The log carries names (the vocabulary faces ride as ops); it does not carry
/// types or key layout — the payload is positional and the key is a fixed-width
/// concatenation with no field boundaries. This file is that half, and without
/// it the log is readable as bytes and useless as columns.
///
/// Idempotent for the same schema, and a REFUSAL for a different one at the same
/// version: two shapes behind one number means the number stopped meaning
/// anything (ADR-0042), and a reader that guessed which one it got would be
/// wrong half the time.
#[cfg(feature = "schema-serde")]
pub fn write_sidecar(
    dir: &Path,
    schema: &crate::model::schema::CollectionSchema,
) -> Result<PathBuf> {
    let name = sidecar_name(schema.layout_version);
    let path = dir.join(&name);
    let json = serde_json::to_string_pretty(schema).map_err(|e| Error::Changelog {
        what: format!("{name}: {e}"),
    })?;
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == json => return Ok(path),
        Ok(_) => {
            return Err(Error::Changelog {
                what: format!(
                    "{}: a DIFFERENT schema is already stamped for layout version {} — \
                     a shape change needs a new version (ADR-0042)",
                    path.display(),
                    schema.layout_version
                ),
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&path, e)),
    }
    // Write-then-rename: a reader must never see half a schema.
    let tmp = dir.join(format!("{name}.partial"));
    std::fs::write(&tmp, &json).map_err(|e| io_err(&tmp, e))?;
    std::fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))?;
    Ok(path)
}

/// Sealed segments of one table, in name (= write) order.
pub fn sealed_segments(root: &Path, header: &[u8]) -> Result<Vec<PathBuf>> {
    let dir = table_dir(root, header);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(io_err(&dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some(SEALED_EXT) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Read a SEALED segment: an incomplete record anywhere is corruption, not an
/// end (`seal` is a rename, so a sealed file was complete when it got its name).
pub fn read_sealed(path: &Path) -> Result<Vec<OpFrame>> {
    frames_of(path, true)
}

/// Read a segment that is still being written: stop at the first incomplete
/// record — a torn tail is a normal end, and the frames before it are good.
pub fn read_open_tail(path: &Path) -> Result<Vec<OpFrame>> {
    frames_of(path, false)
}

fn frames_of(path: &Path, strict: bool) -> Result<Vec<OpFrame>> {
    let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let Some(prefix) = bytes.get(pos..pos + 4) else {
            if pos != bytes.len() && strict {
                return Err(corrupt(path, "trailing bytes without a length prefix"));
            }
            return Ok(out);
        };
        let [a, b, c, d] = [prefix[0], prefix[1], prefix[2], prefix[3]];
        let len = u32::from_be_bytes([a, b, c, d]) as usize;
        let body_at = pos + 4;
        let Some(body) = bytes.get(body_at..body_at.saturating_add(len)) else {
            if strict {
                return Err(corrupt(path, "record length runs past the end of the file"));
            }
            return Ok(out);
        };
        match OpFrame::decode(body) {
            Some(frame) => out.push(frame),
            None => return Err(corrupt(path, "record body is not a valid op frame")),
        }
        pos = body_at + len;
    }
}

/// Which entries of a batch this log carries (ADR-0045 §3): the truth half, by
/// slot, failing closed on anything unrecognised.
fn carried(header: &[u8], key: &[u8]) -> bool {
    let Some(rest) = key.strip_prefix(header) else {
        // Another table's key in a cross-collection batch: this directory is
        // this header's, so it is not ours to write.
        return false;
    };
    let Some(&first) = rest.first() else {
        return false;
    };
    let (slot, width) = if first <= FIXED_FACE_MAX as u8 {
        (Slot::from(first), 1usize)
    } else {
        let Some(&second) = rest.get(1) else {
            return false;
        };
        (u16::from_be_bytes([first, second]), 2usize)
    };
    if rest.len() == width {
        // A face's bare prefix is a singleton (a vocabulary's allocation
        // cursor, the table's meta row): no row has that shape.
        return false;
    }
    if slot <= FIXED_FACE_MAX {
        return matches!(
            slot,
            PRIMARY_SLOT
                | DYNAMIC_SLOT
                | FIELD_VOCAB_ID_SLOT
                | FIELD_VOCAB_NAME_SLOT
                | PREDICATE_VOCAB_ID_SLOT
                | PREDICATE_VOCAB_NAME_SLOT
        );
    }
    // Counting families: the two categorical-vocabulary directions are truth
    // (the payload holds ids, ADR-0032); index/reduce/junction are derived.
    matches!(slot >> 12, segment::VOCAB_LABEL | segment::VOCAB_ID)
}

/// The end of the first length-prefixed record in `bytes`, if one is complete.
fn first_record_end(bytes: &[u8]) -> Option<usize> {
    let prefix = bytes.get(..4)?;
    let [a, b, c, d] = [prefix[0], prefix[1], prefix[2], prefix[3]];
    let len = u32::from_be_bytes([a, b, c, d]) as usize;
    4usize.checked_add(len).filter(|end| *end <= bytes.len())
}

fn next_segment(dir: &Path) -> Result<u64> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(dir, e)),
    };
    let mut max: Option<u64> = None;
    for entry in entries {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((stem, _)) = name.split_once('.') else {
            continue;
        };
        let Ok(number) = stem.parse::<u64>() else {
            continue;
        };
        max = Some(max.map_or(number, |m: u64| m.max(number)));
    }
    Ok(max.map_or(0, |m| m + 1))
}

fn create_segment(dir: &Path, segment: u64) -> Result<File> {
    let path = dir.join(format!("{segment:08}.{PARTIAL_EXT}"));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| io_err(&path, e))
}

fn hex(header: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(header.len() * 2);
    for byte in header {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn io_err(path: &Path, e: std::io::Error) -> Error {
    Error::Changelog {
        what: format!("{}: {e}", path.display()),
    }
}

fn corrupt(path: &Path, what: &str) -> Error {
    Error::Changelog {
        what: format!("{}: {what}", path.display()),
    }
}
