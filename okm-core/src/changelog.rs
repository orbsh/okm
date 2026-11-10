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
//! Format 2 wraps that record stream so a segment knows what it is (ADR-0056 §7):
//! a header at open (`OKMS`, format, kind, layout version, the table's header and
//! name, the declaration's face fingerprint) and a footer at seal (`body length |
//! frames | ordinal | crc32 | OKME`). Both are optional to a reader: a file
//! without the magic is format 1, a bare record stream, and stays readable. The
//! checksum is computed when the file is sealed — one extra read at rotation, not
//! a per-write cost.
//!
//! **What is captured is the TRUTH half of the batch** — primary rows (slot
//! `0x00`), the dynamic segment (`0x01`), the two dynamic field-name faces
//! (`0x02`/`0x03`), the two predicate faces (`0x04`/`0x05`), and both directions
//! of every declared categorical vocabulary (families `0x1`/`0x2`, ADR-0032:
//! the payload stores ids, so the labels are truth, not decoration). The DERIVED
//! half is dropped — declared indexes (`0x3nnn`), reduces (`0x4nnn`), lead
//! faces (`0x5nnn`, ADR-0054), the graph's traversal faces (`0x06`–`0x09`, rebuilt from the
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
use crate::model::faces::FINGERPRINT_LEN;
use crate::model::index::{FIXED_FACE_MAX, Slot, segment};
use crate::model::schema::SlotMap;

/// Which side of the capture rule a slot falls on (ADR-0056 §2).
///
/// `Truth` is what the log keeps: the primary row, the dynamic segment, the four
/// vocabulary directions and both categorical families. `Derived` is everything a
/// reader can rebuild by replaying the truth through a write path — declared
/// indexes, reduce accumulators, lead faces, the graph's traversal faces, the
/// vector snapshot, the table meta row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Truth,
    Derived,
}

/// Classify ONE slot. The six fixed truth numbers come from the declaration's
/// [`SlotMap`] (they are not constants a capture filter should re-type), and the
/// remaining numbering space is classified by family, so every value is answered
/// instead of falling through a whitelist.
///
/// This is the single place the split is written: [`Changelog::stage`] drops what
/// this calls derived, and the drainer reads the same answer instead of matching
/// on the same numbers again.
pub fn side_of(slots: &SlotMap, slot: u16) -> Side {
    if slot == slots.primary
        || slot == slots.dynamic
        || slot == slots.field_vocab_id
        || slot == slots.field_vocab_name
        || slot == slots.predicate_vocab_id
        || slot == slots.predicate_vocab_name
    {
        return Side::Truth;
    }
    // Counting families: the two categorical-vocabulary directions are truth (the
    // payload holds ids, ADR-0032); index, reduce and lead are derived.
    match slot >> 12 {
        segment::VOCAB_LABEL | segment::VOCAB_ID => Side::Truth,
        _ => Side::Derived,
    }
}

/// Extension of a segment still being written. Readers ignore it: its tail may
/// be torn (ADR-0045 §5).
pub const PARTIAL_EXT: &str = "partial";
/// Extension of a sealed segment: immutable, ordered by name, drainable.
pub const SEALED_EXT: &str = "segment";

/// Magic at the head of a self-describing segment (format 2). A file without it
/// is format 1 — a bare record stream — which this reader still accepts.
pub const MAGIC: &[u8; 4] = b"OKMS";
/// Magic at the tail, written after the footer fields: "this file ends HERE".
pub const TRAILER: &[u8; 4] = b"OKME";
/// The container format this writer emits.
pub const FORMAT: u8 = 2;
/// A segment of committed writes (ADR-0045).
pub const KIND_DELTA: u8 = 1;
/// A segment that IS the table's state at one moment (ADR-0056 §3).
pub const KIND_BASELINE: u8 = 2;

/// What a segment says about itself before its first record (ADR-0056 §7).
///
/// A format-1 segment is only readable inside its directory: the table is the
/// directory's name in hex, the shape is the sibling `schema-vN.json`, and
/// neither travels with the file. This header is the file's own answer to "which
/// table, which shape, which kind of stream" — the three facts a reader needs
/// before it can refuse the wrong file instead of misparsing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentMeta {
    /// [`KIND_DELTA`] or [`KIND_BASELINE`].
    pub kind: u8,
    /// The layout version the body's payloads were written under (ADR-0042).
    pub layout_version: u8,
    /// The table's declared name (`#[ok_name]`, ADR-0034). Empty = unnamed.
    pub name: String,
    /// The declaration's face fingerprint (ADR-0048): ties the file to the
    /// sidecar a reader finds beside it.
    pub fingerprint: [u8; crate::model::faces::FINGERPRINT_LEN],
}

impl SegmentMeta {
    /// The meta a writer of `<R>` stamps on a segment of `kind`.
    pub fn of<R: crate::Document>(kind: u8) -> Self {
        Self {
            kind,
            layout_version: R::LAYOUT_VERSION,
            name: R::TABLE_NAME.to_string(),
            fingerprint: R::FACES,
        }
    }
}

/// What one segment file declares, without decoding a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentInfo {
    /// `None` for a format-1 file (a bare record stream).
    pub meta: Option<SegmentMeta>,
    /// The table header the file was stamped with, so a reader can check it
    /// against the directory instead of trusting the path (empty for format 1).
    pub header: Vec<u8>,
    /// The segment's number as written in its footer (0 for format 1).
    pub ordinal: u64,
    /// Frames the footer counts (0 for format 1).
    pub frames: u64,
}

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
    /// What this table's segments say about themselves (ADR-0056 §7).
    meta: SegmentMeta,
    /// The declaration's slot numbers, so the capture rule reads them instead of
    /// re-typing the constants (ADR-0056 §2).
    slots: SlotMap,
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
    /// Frames written into the CURRENT segment (the footer's count).
    frames: u64,
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
    pub fn open(cfg: ChangelogConfig, header: &[u8], meta: SegmentMeta) -> Result<Self> {
        let dir = table_dir(&cfg.dir, header);
        std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        let segment = next_segment(&dir)?;
        let mut file = create_segment(&dir, segment)?;
        let stamp = header_bytes(header, &meta);
        file.write_all(&stamp).map_err(|e| io_err(&dir, e))?;
        Ok(Self {
            dir,
            header: header.to_vec(),
            meta,
            slots: SlotMap::standard(),
            cfg,
            file: Some(file),
            segment,
            written: 0,
            opened: Instant::now(),
            pending: Vec::new(),
            staged: Vec::new(),
            frames: 0,
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
            .filter(|(key, _)| carried(&self.slots, &self.header, key))
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
            if !carried(&self.slots, &self.header, key) {
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
        self.frames += 1;
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
        // Stamp the file before it gets its name: the checksum is one extra READ
        // of the segment, at seal, and never on the write path (ADR-0056 §7).
        self.stamp_footer()?;
        std::fs::rename(self.partial_path(), self.sealed_path())
            .map_err(|e| io_err(&self.partial_path(), e))?;
        self.segment += 1;
        self.written = 0;
        self.frames = 0;
        self.opened = Instant::now();
        let mut file = create_segment(&self.dir, self.segment)?;
        let stamp = header_bytes(&self.header, &self.meta);
        file.write_all(&stamp)
            .map_err(|e| io_err(&self.dir, e))?;
        self.file = Some(file);
        Ok(())
    }

    /// Append the footer to the open segment: body length, frame count, ordinal,
    /// checksum and the tail magic. A footer written just before the rename is
    /// as atomic as the rename itself, which is why the file never carries a
    /// half-stamped body under a `.segment` name.
    fn stamp_footer(&mut self) -> Result<()> {
        let path = self.partial_path();
        let bytes = std::fs::read(&path).map_err(|e| io_err(&path, e))?;
        let start = header_bytes(&self.header, &self.meta).len();
        let body = bytes.get(start..).unwrap_or(&[]);
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes());
        out.extend_from_slice(&self.frames.to_be_bytes());
        out.extend_from_slice(&self.segment.to_be_bytes());
        out.extend_from_slice(&crc32(body).to_be_bytes());
        out.extend_from_slice(TRAILER);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| io_err(&path, e))?;
        file.write_all(&out).map_err(|e| io_err(&path, e))?;
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
            // Stamp before the rename: an unstamped file under a `.segment` name
            // would be a sealed segment every reader refuses.
            let _ = self.stamp_footer();
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

/// The bytes a format-2 segment starts with (ADR-0056 §7):
/// `magic | fmt | kind | layout_version | hdr_len | header | name_len | name | fingerprint`.
///
/// The table header rides inside the file because the directory name is the only
/// other copy, and a copied segment carries no directory. Lengths are clamped
/// rather than rejected: a name longer than a u16 is a declaration bug the export
/// would already have refused, and a segment that cannot be stamped is worse than
/// one with a shortened name.
fn header_bytes(header: &[u8], meta: &SegmentMeta) -> Vec<u8> {
    let header = &header[..header.len().min(u8::MAX as usize)];
    let name = meta.name.as_bytes();
    let name = &name[..name.len().min(u16::MAX as usize)];
    let mut out = Vec::with_capacity(8 + header.len() + 2 + name.len() + FINGERPRINT_LEN);
    out.extend_from_slice(MAGIC);
    out.push(FORMAT);
    out.push(meta.kind);
    out.push(meta.layout_version);
    out.push(header.len() as u8);
    out.extend_from_slice(header);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&meta.fingerprint);
    out
}

/// A parsed segment: where its body is, and what it says about itself. `meta` is
/// `None` for a format-1 file, which carries no header at all.
struct Parsed<'a> {
    body: &'a [u8],
    meta: Option<SegmentMeta>,
    header: Vec<u8>,
    ordinal: u64,
    frames: u64,
}

fn parse<'a>(path: &Path, bytes: &'a [u8], strict: bool) -> Result<Parsed<'a>> {
    let Some(after_magic) = bytes.strip_prefix(MAGIC.as_slice()) else {
        // Format 1: a bare record stream. Shipped logs stay readable.
        return Ok(Parsed {
            body: bytes,
            meta: None,
            header: Vec::new(),
            ordinal: 0,
            frames: 0,
        });
    };
    let mut at = 0usize;
    let fmt = field(path, after_magic, &mut at, 1)?[0];
    if fmt != FORMAT {
        return Err(corrupt(
            path,
            &format!("segment format {fmt} is not {FORMAT}"),
        ));
    }
    let kind = field(path, after_magic, &mut at, 1)?[0];
    let layout_version = field(path, after_magic, &mut at, 1)?[0];
    let hdr_len = field(path, after_magic, &mut at, 1)?[0] as usize;
    let header = field(path, after_magic, &mut at, hdr_len)?.to_vec();
    let name_len = be_u16(path, field(path, after_magic, &mut at, 2)?)? as usize;
    let name = String::from_utf8_lossy(field(path, after_magic, &mut at, name_len)?).into_owned();
    let mut fingerprint = [0u8; FINGERPRINT_LEN];
    fingerprint.copy_from_slice(field(path, after_magic, &mut at, FINGERPRINT_LEN)?);
    let meta = SegmentMeta {
        kind,
        layout_version,
        name,
        fingerprint,
    };
    // `at` counts from the end of the magic; every slice into the file is `+4`.
    let body_at = MAGIC.len() + at;

    // The footer is the last 32 bytes, present only on a sealed file.
    if !bytes.ends_with(TRAILER.as_slice()) || bytes.len() < body_at + 32 {
        if strict {
            return Err(corrupt(path, "a sealed segment has no footer"));
        }
        return Ok(Parsed {
            body: bytes.get(body_at..).unwrap_or(&[]),
            meta: Some(meta),
            header,
            ordinal: 0,
            frames: 0,
        });
    }
    let foot = bytes.get(bytes.len() - 32..).unwrap_or(&[]);
    let body_len = be_u64(path, foot.get(..8).unwrap_or(&[]))? as usize;
    let frames = be_u64(path, foot.get(8..16).unwrap_or(&[]))?;
    let ordinal = be_u64(path, foot.get(16..24).unwrap_or(&[]))?;
    let crc = be_u32(path, foot.get(24..28).unwrap_or(&[]))?;
    let body = bytes.get(body_at..bytes.len() - 32).unwrap_or(&[]);
    if body.len() != body_len {
        return Err(corrupt(
            path,
            "the footer's body length disagrees with the file",
        ));
    }
    if crc32(body) != crc {
        return Err(corrupt(
            path,
            "checksum mismatch — this file is not what was sealed",
        ));
    }
    Ok(Parsed {
        body,
        meta: Some(meta),
        header,
        ordinal,
        frames,
    })
}

fn field<'a>(path: &Path, bytes: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = at
        .checked_add(n)
        .ok_or_else(|| corrupt(path, "header field overflows"))?;
    let slice = bytes
        .get(*at..end)
        .ok_or_else(|| corrupt(path, "header runs past the end of the file"))?;
    *at = end;
    Ok(slice)
}

fn be_u16(path: &Path, bytes: &[u8]) -> Result<u16> {
    let arr: [u8; 2] = bytes
        .try_into()
        .map_err(|_| corrupt(path, "a u16 field is not two bytes"))?;
    Ok(u16::from_be_bytes(arr))
}

fn be_u32(path: &Path, bytes: &[u8]) -> Result<u32> {
    let arr: [u8; 4] = bytes
        .try_into()
        .map_err(|_| corrupt(path, "a u32 field is not four bytes"))?;
    Ok(u32::from_be_bytes(arr))
}

fn be_u64(path: &Path, bytes: &[u8]) -> Result<u64> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| corrupt(path, "a u64 field is not eight bytes"))?;
    Ok(u64::from_be_bytes(arr))
}

/// CRC-32 (IEEE), so a short copy or a flipped byte is refused instead of
/// replayed. The table is built at compile time; the check itself runs at seal
/// and on every read, never on the write path.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        let idx = ((crc ^ u32::from(*byte)) & 0xFF) as usize;
        crc = CRC_TABLE[idx] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// What a segment file declares about itself, without decoding a record. A sealed
/// file is verified (footer + checksum); a `.partial` is read for its header only.
pub fn segment_info(path: &Path) -> Result<SegmentInfo> {
    let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
    let strict = path.extension().and_then(|e| e.to_str()) == Some(SEALED_EXT);
    let parsed = parse(path, &bytes, strict)?;
    Ok(SegmentInfo {
        meta: parsed.meta,
        header: parsed.header,
        ordinal: parsed.ordinal,
        frames: parsed.frames,
    })
}

fn frames_of(path: &Path, strict: bool) -> Result<Vec<OpFrame>> {
    let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
    let parsed = parse(path, &bytes, strict)?;
    decode_records(path, parsed.body, strict)
}

fn decode_records(path: &Path, bytes: &[u8], strict: bool) -> Result<Vec<OpFrame>> {
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
fn carried(slots: &SlotMap, header: &[u8], key: &[u8]) -> bool {
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
    side_of(slots, slot) == Side::Truth
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
