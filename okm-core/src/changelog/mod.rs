//! The change log (ADR-0045): capture at the commit boundary, drain out of
//! process.
//!
//! A collection over a capturing store ([`ChangelogStore`]) appends ONE frame per
//! committed batch to a segment file:
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
//! a header at open (format, kind, layout version, the table's header and name,
//! the declaration's face fingerprint) and a footer at seal (`body length |
//! frames | ordinal | crc32`). The footer is the one thing a sealed file has and
//! a `.partial` does not — the extension is the seal, so a `strict` reader
//! requires it and verifies the checksum, which is computed at rotation, not per
//! write.
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

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::OpFrame;
use crate::engine::storage::{Batch, VirtualStorage};
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
/// Who may write a table's log, and where the last writer stopped (ADR-0056 §6).
pub mod owner;

/// The window join: the drain's reading half, beside the segments it reads.
///
/// Gated on `schema-serde`, because a window cannot be read without a
/// declaration and a declaration on disk IS the JSON sidecar (ADR-0056 §7). The
/// segments themselves are readable without it: `read_sealed` answers ops and
/// bytes, which is all a raw consumer needs.
#[cfg(feature = "schema-serde")]
pub mod replay;
#[cfg(feature = "schema-serde")]
pub mod window;

/// Extension of a sealed segment: immutable, ordered by name, drainable.
pub const SEALED_EXT: &str = "segment";

/// File-name suffix of a snapshot segment (`00000007.baseline.segment`): the kind
/// is in the name so a reader finds a table's baseline without opening a segment,
/// and the header carries it as well (ADR-0056 §7).
pub const BASELINE_SUFFIX: &str = ".baseline.segment";

/// How many truth-half ops one baseline frame carries. A frame is one length
/// prefix, so the chunk bounds the record and keeps a big table from building a
/// single 4 GiB-sized buffer in memory.
const BASELINE_CHUNK: usize = 1024;

/// The container format this writer emits, and the first byte of a segment.
pub const FORMAT: u8 = 2;
/// How many bytes the footer occupies at the end of a sealed file.
const FOOTER_LEN: usize = 28;
/// A segment of committed writes (ADR-0045).
pub const KIND_DELTA: u8 = 1;
/// A segment that IS the table's state at one moment (ADR-0056 §3).
pub const KIND_BASELINE: u8 = 2;

/// What a segment says about itself before its first record (ADR-0056 §7).
///
/// Without this header a segment is only readable inside its directory: the
/// table is the directory's name in hex, the shape is the sibling
/// `schema-vN.json`, and neither travels with the file. This header is the file's own answer to "which
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
    /// The same table's meta for another `kind` — a baseline beside the deltas that
    /// declared it.
    pub fn with_kind(&self, kind: u8) -> Self {
        Self {
            kind,
            ..self.clone()
        }
    }

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
    /// What the header declares: kind, layout version, name, fingerprint.
    pub meta: SegmentMeta,
    /// The table header the file was stamped with, so a reader can check it
    /// against the directory instead of trusting the path.
    pub header: Vec<u8>,
    /// The segment's number as written in its footer (0 while unsealed).
    pub ordinal: u64,
    /// Frames the footer counts (0 while unsealed).
    pub frames: u64,
}

/// How much of the change log to keep in flight (ADR-0045 §12).
///
/// The first three knobs are the writer's rotation policy. The export window is
/// the DRAINER's policy and is deliberately not configurable here: one segment
/// is a rotation unit, not a file the sink has to accept.
/// Where a table's log lives — or that there is none (ADR-0056 §6).
///
/// `Local` means a POSIX directory: a disk, a bind mount, or an NFS mount. Not "a place
/// with a path": the protocol needs atomic rename (a seal IS a rename), fsync (a tail
/// that reached the log before a handover was announced), and a file that two processes
/// can both read. Anywhere those are missing is a COPY target, not a log — which is what
/// the Delta export is for, and why the sink stays a backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    /// No capture at all. The store keeps the wrapper (assembly does not fork), nothing
    /// is opened, nothing is written, and the cost is one branch per commit.
    Off,
}

/// When the store compacts its own log (ADR-0056 §6).
///
/// Checked at a rotation and nowhere else: that is where the writer is already at a
/// boundary, already paying for an fsync, and already the one that may write. The work it
/// can trigger is one scan of the table's truth half, so the thresholds are meant to be
/// generous — this bounds the log's size and its replay time, it is not a maintenance
/// daemon. `after_secs` is read from the newest baseline's mtime, and it is checked
/// lazily: core has no clock and no scheduler (ADR-0045 §7), so a quiet table compacts at
/// its next write, or when the host calls [`ChangelogStore::compact`] itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactPolicy {
    /// Uncovered bytes — everything above the newest baseline — that trigger a compaction.
    pub after_bytes: u64,
    /// Age of the newest baseline that triggers one. With no baseline yet, the age of the
    /// oldest segment answers, so a table that has been writing for a day gets its first.
    pub after_secs: u64,
    /// Segments below the new baseline to keep, as a window for a human. 0 keeps none.
    pub keep: usize,
}

impl Default for CompactPolicy {
    fn default() -> Self {
        Self {
            after_bytes: 1 << 30,
            after_secs: 24 * 60 * 60,
            keep: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChangelogConfig {
    /// The log's root, or `Off`. Each table header gets its own subdirectory, so
    /// streams cannot collide.
    pub location: Location,
    /// When the log compacts itself, if ever. `None` = never automatically; the verb
    /// stays available.
    pub compact: Option<CompactPolicy>,
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
            location: Location::Local(dir.into()),
            compact: Some(CompactPolicy::default()),
            segment_bytes: Self::DEFAULT_SEGMENT_BYTES,
            segment_seconds: Self::DEFAULT_SEGMENT_SECONDS,
            buffer_bytes: Self::DEFAULT_BUFFER_BYTES,
            sync_data: false,
        }
    }

    /// Capture off: the same assembly, no log. The branch is the whole cost.
    pub fn off() -> Self {
        Self {
            location: Location::Off,
            compact: None,
            ..Self::new(PathBuf::new())
        }
    }

    /// The root directory, or `None` when capture is off.
    pub fn root(&self) -> Option<&Path> {
        match &self.location {
            Location::Local(dir) => Some(dir),
            Location::Off => None,
        }
    }

    /// The root, or the one error every writer path shares.
    fn root_or_err(&self) -> Result<&Path> {
        self.root().ok_or_else(|| Error::Changelog {
            what: "capture is Off; there is no log to write".into(),
        })
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

/// What a writer hands over when it stops (ADR-0056 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drained {
    /// The last sealed segment in the directory — the position a READER can see, which
    /// is what the next writer has to reach before it may serve.
    pub sealed: Option<String>,
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
    /// Set when a seal actually renamed a file. Read and cleared by the store, which is
    /// the only place that can act on it: compaction needs the store, not one writer.
    rotated: bool,
    /// The cutover switch (ADR-0056 §6): `false` means this writer lost the log and
    /// must not append to it. `None` (the default) is a single-writer process, which
    /// pays one `Option` check and nothing else.
    gate: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Changelog {
    /// Open the table's directory and start the next segment.
    ///
    /// An existing `.partial` is LEFT ALONE, never appended to: its tail may be
    /// torn, and a writer cannot tell where the last good frame ends without
    /// reading it (which the drainer does). Starting a fresh segment number is
    /// what keeps a crash from corrupting a stream that already has readers.
    pub fn open(cfg: ChangelogConfig, header: &[u8], meta: SegmentMeta) -> Result<Self> {
        let root = cfg.root_or_err()?;
        let dir = table_dir(root, header);
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
            rotated: false,
            gate: None,
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
        // A closed gate means another process holds the log now. The commit that got
        // here is already in the store, so the log simply does not hear about it: the
        // log is a hint (ADR-0045 §6), and the reconciliation scan at cutover is what
        // covers the gap (ADR-0056 §4).
        if let Some(gate) = &self.gate
            && !gate.load(std::sync::atomic::Ordering::SeqCst)
        {
            self.staged.clear();
            self.dropped += 1;
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

    /// Give this writer the cutover switch (ADR-0056 §6). The caller holds the lease and
    /// reads it with `Lease::gate`; the writer reads the gate at every commit and stops
    /// appending the moment it closes.
    pub fn attach_gate(&mut self, gate: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.gate = Some(gate);
    }

    /// Stop and hand over: seal what is pending, wait for the sink's copies, and answer
    /// the position a reader can see (ADR-0056 §6).
    ///
    /// This is the verb `Drop` is not: `Drop`'s drain is best-effort and cannot report a
    /// failure, so a cutover built on it would be built on an assumption. Here a failed
    /// flush or an unconfirmable upload is visible to the caller, which is what makes
    /// "the old writer is fenced" a fact rather than a hope.
    pub fn drain(&mut self) -> Result<Drained> {
        if self.written > 0 || !self.pending.is_empty() {
            self.seal_last()?;
        } else if self.frame_count_is_empty() {
            // Nothing was written: close the handle and take the empty `.partial` with
            // it, so a handover does not leave a file a reader has to ignore.
            self.file = None;
            let _ = std::fs::remove_file(self.partial_path());
        }
        Ok(Drained {
            sealed: last_sealed(&self.dir),
        })
    }

    /// Whether the open segment carries no frames at all (an `open` that never
    /// committed). Split out so `drain` reads like the decision it is.
    fn frame_count_is_empty(&self) -> bool {
        self.frames == 0 && self.written == 0
    }

    /// Seal the current segment now — the caller's rotation, for a host that
    /// wants a window boundary at a moment of its choosing.
    pub fn seal(&mut self) -> Result<()> {
        self.seal_last()?;
        self.segment += 1;
        self.opened = Instant::now();
        let mut file = create_segment(&self.dir, self.segment)?;
        let stamp = header_bytes(&self.header, &self.meta);
        file.write_all(&stamp).map_err(|e| io_err(&self.dir, e))?;
        self.file = Some(file);
        Ok(())
    }

    /// Seal the segment and STOP: no fresh segment is opened, so a one-shot writer
    /// (a baseline, ADR-0056 §3) leaves exactly one file behind and the next
    /// `open` takes the same number again — no hole in the numbering.
    pub fn seal_last(&mut self) -> Result<()> {
        self.write_pending()?;
        self.file = None;
        // Stamp the file before it gets its name: the checksum is one extra READ
        // of the segment, at seal, and never on the write path (ADR-0056 §7).
        self.stamp_footer()?;
        let sealed = self.sealed_path();
        std::fs::rename(self.partial_path(), &sealed)
            .map_err(|e| io_err(&self.partial_path(), e))?;
        self.written = 0;
        self.frames = 0;
        self.rotated = true;
        Ok(())
    }

    /// Did this writer seal a segment since the last time the store asked?
    fn took_rotation(&mut self) -> bool {
        std::mem::take(&mut self.rotated)
    }

    /// Append the footer to the open segment: body length, frame count, ordinal
    /// and checksum. A footer written just before the rename is as atomic as the
    /// rename itself, which is why the file never carries a half-stamped body
    /// under a `.segment` name.
    fn stamp_footer(&mut self) -> Result<()> {
        let path = self.partial_path();
        let bytes = std::fs::read(&path).map_err(|e| io_err(&path, e))?;
        let start = header_bytes(&self.header, &self.meta).len();
        let body = bytes.get(start..).unwrap_or(&[]);
        let mut out = Vec::with_capacity(FOOTER_LEN);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes());
        out.extend_from_slice(&self.frames.to_be_bytes());
        out.extend_from_slice(&self.segment.to_be_bytes());
        out.extend_from_slice(&crc32(body).to_be_bytes());
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
        // The kind is in the FILE NAME so a reader finds a table's snapshot
        // without opening every segment; the header carries it too (ADR-0056 §7).
        let name = if self.meta.kind == KIND_BASELINE {
            format!("{:08}.baseline.{}", self.segment, SEALED_EXT)
        } else {
            format!("{:08}.{}", self.segment, SEALED_EXT)
        };
        self.dir.join(name)
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
            let sealed = self.sealed_path();
            let _ = std::fs::rename(self.partial_path(), &sealed);
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

/// The baseline segments of one table, in name order (empty when it has none).
///
/// A baseline is `kind = baseline`: the table's whole truth half at one moment.
/// Its order matters to a replay — apply the newest one, then every segment
/// numbered above it (ADR-0056 §3).
pub fn baseline_segments(root: &Path, header: &[u8]) -> Result<Vec<PathBuf>> {
    let dir = table_dir(root, header);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(&dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(BASELINE_SUFFIX) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Does this table already have a snapshot segment?
fn has_baseline(dir: &Path) -> Result<bool> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        let name = entry.file_name();
        if let Some(name) = name.to_str()
            && name.ends_with(BASELINE_SUFFIX)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Sealed DELTA segments of one table, in name (= write) order. A baseline is not
/// here: it is a snapshot rather than a window of changes, and
/// [`baseline_segments`] is where a replay looks for it.
/// The owner module answers `String` (it has no dependency on the crate's error type);
/// every call site turns it into the one error a log can return.
fn owner_err(what: String) -> Error {
    Error::Changelog { what }
}

/// A table's log as a policy (or an operator) sees it. Sizes come from the files, so the
/// answer survives a restart and needs no bookkeeping in the writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogShape {
    pub segments: usize,
    /// Every segment, baselines included.
    pub bytes: u64,
    /// The newest baseline, if the table has ever been compacted (or captured from a store
    /// that had one).
    pub baseline: Option<String>,
    /// How old that baseline is. With none, how old the OLDEST segment is — a table that has
    /// been writing for a day without a checkpoint is exactly the case the policy is for.
    pub baseline_age_secs: Option<u64>,
    /// Bytes the newest baseline does not cover: what a replay since the baseline would read.
    pub uncovered: u64,
}

/// Read a table's directory.
pub fn log_shape(dir: &Path) -> Result<LogShape> {
    let mut newest_baseline: Option<String> = None;
    let mut newest_baseline_at: Option<std::time::SystemTime> = None;
    let mut oldest_at: Option<std::time::SystemTime> = None;
    let mut files: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| io_err(dir, e))? {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".segment") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let at = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        oldest_at = Some(match oldest_at {
            Some(prev) if prev <= at => prev,
            _ => at,
        });
        if name.contains(".baseline.")
            && newest_baseline
                .as_ref()
                .is_none_or(|prev| name.as_str() > prev.as_str())
        {
            newest_baseline = Some(name.clone());
            newest_baseline_at = Some(at);
        }
        files.push((name, meta.len(), at));
    }
    let uncovered = match &newest_baseline {
        Some(baseline) => files
            .iter()
            .filter(|(name, _, _)| name.as_str() > baseline.as_str())
            .map(|(_, len, _)| *len)
            .sum(),
        None => files.iter().map(|(_, len, _)| *len).sum(),
    };
    let age_secs = newest_baseline_at.or(oldest_at).map(|at| {
        std::time::SystemTime::now()
            .duration_since(at)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });
    Ok(LogShape {
        segments: files.len(),
        bytes: files.iter().map(|(_, len, _)| *len).sum(),
        baseline: newest_baseline,
        baseline_age_secs: age_secs,
        uncovered,
    })
}

/// What one table's compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// The baseline that now covers everything below it.
    pub baseline: Option<String>,
    /// Segments deleted because the baseline covered them.
    pub removed: usize,
    /// Their size on disk, so a host can log what it got back.
    pub freed: u64,
}

/// What a byte-level replay did: how many ops it applied, and the last segment it read
/// (the position a reader reaches, which is what a handover has to be compared against).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub ops: usize,
    pub last: Option<String>,
}

/// Apply a table's log to a store, BYTE FOR BYTE — the same-build half of a migration
/// (ADR-0056 §6).
///
/// No decode and no re-encode: a segment already carries the final key and payload, so
/// `put`/`del` in log order reproduces the table. That is exactly what makes it the
/// wrong tool for a LAYOUT change — those go through the decoder and the target's write
/// path (ADR-0057 §4) — and the right one for a blue/green restart of one build.
///
/// `after` names the last segment already applied; `None` applies the whole log,
/// baseline included, in name order (a baseline's number puts it where it happens, so an
/// old snapshot can never resurrect rows a later segment deleted).
pub fn apply_log<S: crate::VirtualStorage>(
    root: &Path,
    header: &[u8],
    target: &S,
    after: Option<&str>,
) -> Result<Applied> {
    let segments = all_segments(root, header)?;
    let mut applied = Applied { ops: 0, last: None };
    for path in segments {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if let Some(after) = after
            && name.as_str() <= after
        {
            continue;
        }
        for frame in read_sealed(&path)? {
            for (tag, key, value) in frame.0 {
                match tag {
                    crate::OP_PUT => target.put(key, value)?,
                    crate::OP_DELETE => target.del(&key)?,
                    other => {
                        return Err(Error::Changelog {
                            what: format!("{}: unknown op tag {other}", path.display()),
                        });
                    }
                }
                applied.ops += 1;
            }
        }
        applied.last = Some(name);
    }
    Ok(applied)
}

/// One captured operation: the key, and the payload — `None` is a delete.
pub type LoggedOp = (Vec<u8>, Option<Vec<u8>>);

/// The ops a baseline for `header` must contain: that table's truth half, through the very
/// filter capture applies (derived faces are left out — a replay above the baseline
/// rebuilds them).
///
/// Public because compaction needs the STORE, not the log: a log directory alone holds no
/// state to snapshot, which is why a maintenance tool takes the data directory as its
/// second argument.
pub fn baseline_ops<S: VirtualStorage>(
    store: &S,
    header: &[u8],
) -> Result<Vec<LoggedOp>> {
    let slots = SlotMap::standard();
    let mut ops = Vec::new();
    for (suffix, value) in crate::engine::storage::scan_suffix_kv(store, header)? {
        let mut key = header.to_vec();
        key.extend_from_slice(&suffix);
        if carried(&slots, header, &key) {
            ops.push((key, Some(value)));
        }
    }
    Ok(ops)
}

/// Write one baseline segment for `header` from ops already in hand, and say which files it
/// became — `(first, last)` by name. A baseline is written in chunks, so it can be several
/// files, and "covered" means everything numbered below the FIRST of them.
pub fn write_baseline(
    cfg: &ChangelogConfig,
    header: &[u8],
    meta: SegmentMeta,
    ops: &[LoggedOp],
) -> Result<Option<(String, String)>> {
    let Some(root) = cfg.root() else {
        return Ok(None);
    };
    let existed: std::collections::BTreeSet<String> = baseline_segments(root, header)?
        .into_iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let mut log = Changelog::open(cfg.clone(), header, meta)?;
    for chunk in ops.chunks(BASELINE_CHUNK) {
        if log.stage(chunk) > 0 {
            log.commit_staged()?;
        }
    }
    // `seal_last` and not `seal`: a one-shot writer leaves exactly one file, and the next
    // `open` takes the same number again — no hole.
    log.seal_last()?;
    let fresh: Vec<String> = baseline_segments(root, header)?
        .into_iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|name| !existed.contains(name))
        .collect();
    Ok(match (fresh.first(), fresh.last()) {
        (Some(first), Some(last)) => Some((first.clone(), last.clone())),
        _ => None,
    })
}

/// Delete every segment numbered below `baseline`, keeping the newest `keep` of them.
/// Returns how many went and how many bytes they were.
pub fn prune_below(root: &Path, header: &[u8], baseline: &str, keep: usize) -> Result<(usize, u64)> {
    let mut covered = all_segments(root, header)?;
    covered.retain(|path| {
        path.file_name()
            .map(|n| n.to_string_lossy().as_ref() < baseline)
            .unwrap_or(false)
    });
    let cut = covered.len().saturating_sub(keep);
    let mut removed = 0;
    let mut freed = 0;
    for path in covered.into_iter().take(cut) {
        if let Ok(stat) = std::fs::metadata(&path) {
            freed += stat.len();
        }
        std::fs::remove_file(&path).map_err(|e| Error::Changelog {
            what: format!("{}: {e}", path.display()),
        })?;
        removed += 1;
    }
    Ok((removed, freed))
}

/// Every segment of one table — baselines included — in name order.
///
/// Name order is log order: a baseline's number is the number of the segment it outranks,
/// so sorting the two kinds together is what makes a replay correct after a compaction.
fn all_segments(root: &Path, header: &[u8]) -> Result<Vec<PathBuf>> {
    let mut out = baseline_segments(root, header)?;
    out.extend(sealed_segments(root, header)?);
    out.sort();
    Ok(out)
}

/// The last sealed segment in ONE table directory, by name — the ordinal is the name's
/// prefix, so name order is seal order. `None` for a directory that has none yet.
pub fn last_sealed(dir: &Path) -> Option<String> {
    let suffix = format!(".{SEALED_EXT}");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(&suffix))
        .max()
}

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
        let is_segment = path.extension().and_then(|e| e.to_str()) == Some(SEALED_EXT);
        let is_baseline = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(BASELINE_SUFFIX));
        if is_segment && !is_baseline {
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

/// The bytes a segment starts with (ADR-0056 §7):
/// `fmt | kind | layout_version | hdr_len | header | name_len | name | fingerprint`.
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
    let mut out = Vec::with_capacity(4 + header.len() + 2 + name.len() + FINGERPRINT_LEN);
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

/// A parsed segment: where its body is, and what it says about itself.
struct Parsed<'a> {
    body: &'a [u8],
    meta: SegmentMeta,
    header: Vec<u8>,
    ordinal: u64,
    frames: u64,
}

fn parse<'a>(path: &Path, bytes: &'a [u8], strict: bool) -> Result<Parsed<'a>> {
    let mut at = 0usize;
    let fmt = field(path, bytes, &mut at, 1)?[0];
    if fmt != FORMAT {
        return Err(corrupt(
            path,
            &format!("segment format {fmt} is not {FORMAT}"),
        ));
    }
    let kind = field(path, bytes, &mut at, 1)?[0];
    let layout_version = field(path, bytes, &mut at, 1)?[0];
    let hdr_len = field(path, bytes, &mut at, 1)?[0] as usize;
    let header = field(path, bytes, &mut at, hdr_len)?.to_vec();
    let name_len = be_u16(path, field(path, bytes, &mut at, 2)?)? as usize;
    let name = String::from_utf8_lossy(field(path, bytes, &mut at, name_len)?).into_owned();
    let mut fingerprint = [0u8; FINGERPRINT_LEN];
    fingerprint.copy_from_slice(field(path, bytes, &mut at, FINGERPRINT_LEN)?);
    let meta = SegmentMeta {
        kind,
        layout_version,
        name,
        fingerprint,
    };
    // `at` is the body's offset now, and every slice into the file is absolute.
    let body_at = at;

    // Only a sealed file carries a footer; a `.partial` is body-so-far.
    if !strict {
        return Ok(Parsed {
            body: bytes.get(body_at..).unwrap_or(&[]),
            meta,
            header,
            ordinal: 0,
            frames: 0,
        });
    }
    if bytes.len() < body_at + FOOTER_LEN {
        return Err(corrupt(path, "a sealed segment has no footer"));
    }
    let foot = bytes.get(bytes.len() - FOOTER_LEN..).unwrap_or(&[]);
    let body_len = be_u64(path, foot.get(..8).unwrap_or(&[]))? as usize;
    let frames = be_u64(path, foot.get(8..16).unwrap_or(&[]))?;
    let ordinal = be_u64(path, foot.get(16..24).unwrap_or(&[]))?;
    let crc = be_u32(path, foot.get(24..28).unwrap_or(&[]))?;
    let body = bytes.get(body_at..bytes.len() - FOOTER_LEN).unwrap_or(&[]);
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
        meta,
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

/// Capture at the STORE boundary (ADR-0056 §2): one switch for the whole store,
/// a per-table directory whichever collection wrote the batch.
///
/// Why here and not on the collection: the collection-level hook is wired per
/// assembly site, so an instance assembled without it writes rows no log will ever
/// mention, and a caller-owned batch has to remember `commit_batch` or the
/// staged/flush pair to be captured at all. Every write passes `commit_batch`
/// here, so both holes close at once and stage/flush stop being API a caller must
/// know about.
///
/// What it needs from assembly is the DECLARATION ([`ChangelogStore::register`]):
/// the sidecar is the half of the schema the log cannot carry, and a segment is
/// stamped with the name, layout version and face fingerprint. An unregistered
/// table is not captured — a log with no declaration beside it is bytes no reader
/// can turn into columns.
pub struct ChangelogStore<S> {
    inner: S,
    cfg: ChangelogConfig,
    /// header → the declaration it was registered with.
    metas: RefCell<BTreeMap<Vec<u8>, SegmentMeta>>,
    /// header → its writer, opened by the first commit that touches the table.
    tables: RefCell<BTreeMap<Vec<u8>, RefCell<Changelog>>>,
    /// The cutover switch (ADR-0056 §6), handed to every writer this store opens —
    /// including one opened after a takeover request, which is exactly when it matters.
    gate: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The token this store claimed the log under, if it took one over. `compact` needs it
    /// to answer "is this claim mine?" without guessing from a path.
    pub holder: RefCell<Option<String>>,
    /// The claims this store holds. Kept here for the store's lifetime because a dropped
    /// lease stops polling, and a gate nobody polls is a gate that never closes.
    leases: RefCell<Vec<std::sync::Arc<owner::Lease>>>,
}

impl<S> ChangelogStore<S> {
    /// Wrap a store: from here on every commit that touches a registered table is
    /// captured.
    pub fn new(inner: S, cfg: ChangelogConfig) -> Self {
        Self {
            inner,
            cfg,
            metas: RefCell::new(BTreeMap::new()),
            tables: RefCell::new(BTreeMap::new()),
            gate: None,
            holder: RefCell::new(None),
            leases: RefCell::new(Vec::new()),
        }
    }

    /// Give every writer the cutover switch (ADR-0056 §6): closed means this process
    /// lost the log and must stop appending. Applied to the writers already open and to
    /// any opened later.
    pub fn attach_gate(&mut self, gate: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        for cell in self.tables.borrow().values() {
            cell.borrow_mut().attach_gate(std::sync::Arc::clone(&gate));
        }
        self.gate = Some(gate);
    }

    /// Seal every table this store had open and wait for the copies, answering what
    /// each table can offer a reader (ADR-0056 §6).
    ///
    /// A table that was never written has no writer — there is nothing to drain and no
    /// entry to report, which is the same reason `status` answers `None` for it.
    pub fn drain(&self) -> Result<Vec<(Vec<u8>, Drained)>> {
        let mut out = Vec::new();
        for (header, cell) in self.tables.borrow().iter() {
            let drained = cell.borrow_mut().drain()?;
            out.push((header.clone(), drained));
        }
        Ok(out)
    }

    /// The holder's last act, in one call (ADR-0056 §6): seal every open table, wait for
    /// the copies, and record the position a reader can see.
    ///
    /// `holder` and `data` are this instance's, and are written back only for a table
    /// whose claim is `draining` — the state the successor set. A drain with no request
    /// against it seals and reports but claims nothing.
    pub fn drain_and_hand_over(&self, holder: &str, data: &str) -> Result<Vec<(Vec<u8>, Drained)>> {
        let mut out = Vec::new();
        for (header, cell) in self.tables.borrow().iter() {
            let drained = cell.borrow_mut().drain()?;
            let dir = table_dir(self.cfg.root_or_err()?, header);
            if let Some(current) = owner::read(&dir).map_err(owner_err)?
                && current.state == owner::State::Draining
                && let Some(position) = drained.sealed.clone()
            {
                // The position is the sealed segment and nothing else: the successor reads
                // this same directory, so a handover waits on no copy (ADR-0056 §6).
                owner::hand_over(&dir, current.epoch, holder, data, &position)
                    .map_err(owner_err)?;
            }
            out.push((header.clone(), drained));
        }
        Ok(out)
    }

    /// The successor's side, in one call (ADR-0056 §6): ask the holder to stop, wait for
    /// the position it stopped at, apply the log to this store, and only then take the
    /// claim.
    ///
    /// The order matters and is the whole safety argument: the log is not growing while
    /// it is being read, the replay is compared against exactly what was handed over, and
    /// the write switch closes for everyone but the caller when it claims. A mismatch is
    /// an error, never a warning: a successor that cannot reach the handed position has
    /// not caught up, and serving would be the split brain the claim file exists to stop.
    pub fn take_over(
        &mut self,
        holder: &str,
        data: &str,
        timeout: std::time::Duration,
    ) -> Result<Vec<(Vec<u8>, owner::Role)>>
    where
        S: crate::VirtualStorage,
    {
        let root = self.cfg.root_or_err()?.to_path_buf();
        let headers: Vec<Vec<u8>> = self.metas.borrow().keys().cloned().collect();
        let mut out = Vec::new();
        for header in headers {
            let dir = table_dir(&root, &header);
            let role = owner::decide(&dir, holder, data).map_err(owner_err)?;
            if let owner::Role::Shared {
                holder: other,
                epoch,
            } = &role
            {
                return Err(Error::Changelog {
                    what: format!(
                        "{}: store `{data}` is already claimed by `{other}` at epoch {epoch} \
                         — two instances must not share one store",
                        owner::path(&dir).display()
                    ),
                });
            }
            // A handover already recorded answers this: a successor that died between
            // the handover and the claim finds the position waiting for it.
            let handed = match &role {
                owner::Role::Successor { epoch, .. } => {
                    let request = owner::announce_drain(&dir, holder, data).map_err(owner_err)?;
                    debug_assert_eq!(request.epoch, epoch + 1);
                    let position = owner::wait_for_handover(&dir, request.epoch, timeout)
                        .map_err(owner_err)?
                        .ok_or_else(|| Error::Changelog {
                            what: format!(
                                "{}: the holder did not hand over within {timeout:?}",
                                owner::path(&dir).display()
                            ),
                        })?;
                    Some(position)
                }
                owner::Role::HandedOver { position, .. } => {
                    Some(position.clone().ok_or_else(|| Error::Changelog {
                        what: format!(
                            "{}: a handover with no position — nothing to reach",
                            owner::path(&dir).display()
                        ),
                    })?)
                }
                _ => None,
            };
            if let Some(handed) = handed {
                let applied = apply_log(&root, &header, &self.inner, None)?;
                if applied.last.as_deref() != Some(handed.as_str()) {
                    return Err(Error::Changelog {
                        what: format!(
                            "{}: the log ends at {:?}, but the handover named `{handed}` — \
                             replay again before taking the claim",
                            dir.display(),
                            applied.last
                        ),
                    });
                }
            }
            // The guard `claim` applies ("take over, do not claim") is exactly what this
            // call has just satisfied: the log was applied to the handed position.
            let lease = owner::Lease::assume(dir.clone(), holder, data, Default::default())
                .map_err(owner_err)?;
            self.attach_gate(lease.gate());
            self.leases.borrow_mut().push(lease);
            *self.holder.borrow_mut() = Some(holder.to_string());
            out.push((header, role));
        }
        Ok(out)
    }

    /// The log root, so a host can ship it (a backup, a migration). `None` when capture
    /// is [`Location::Off`].
    pub fn dir(&self) -> Option<&Path> {
        self.cfg.root()
    }

    /// The wrapped store.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Unwrap: capture stops here.
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Register a declaration with the switch: write the sidecar (the half of the
    /// schema the log cannot carry) and remember what a segment is stamped with.
    ///
    /// Idempotent per table; two collections over one store register two headers.
    /// A table declared `#[ok_uncaptured]` is accepted and ignored: no sidecar, no
    /// meta, so no writer is ever opened for it and nothing reaches the log.
    pub fn register<K: crate::model::key::KeyEncode, R: crate::model::index::Document<Key = K>>(
        &self,
    ) -> Result<()> {
        if !R::CAPTURED {
            return Ok(());
        }
        let header = R::NS_PREFIX.to_vec();
        let Some(root) = self.cfg.root() else {
            return Ok(());
        };
        let dir = table_dir(root, &header);
        std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        #[cfg(feature = "schema-serde")]
        write_sidecar(&dir, &crate::model::schema::CollectionSchema::of::<K, R>())?;
        self.metas
            .borrow_mut()
            .insert(header, SegmentMeta::of::<R>(KIND_DELTA));
        Ok(())
    }

    /// Write this table's baseline if it has none (ADR-0056 §3); `true` when one
    /// was written.
    ///
    /// A baseline is the table's whole TRUTH half as one `kind = baseline`
    /// segment. It is what makes a log enabled mid-life complete: without one,
    /// everything written before capture is invisible to every consumer, forever.
    ///
    /// Ordering is the whole trick. The table's open writer is sealed and closed
    /// FIRST, so the baseline is numbered after every delta written so far: a
    /// replay applies the newest baseline and then the segments numbered above it,
    /// and everything below is covered by the snapshot. A writer left open would
    /// keep appending to a segment numbered below the baseline, which a replay
    /// would skip — so closing it is the correctness argument, not a tidy-up.
    ///
    /// Costs one bounded pass over the table, once.
    pub fn ensure_baseline<K, R>(&self) -> Result<bool>
    where
        S: VirtualStorage,
        K: crate::model::key::KeyEncode,
        R: crate::model::index::Document<Key = K>,
    {
        if !R::CAPTURED {
            return Ok(false);
        }
        let header = R::NS_PREFIX.to_vec();
        self.register::<K, R>()?;
        let Some(root) = self.cfg.root() else {
            return Ok(false);
        };
        if has_baseline(&table_dir(root, &header))? {
            return Ok(false);
        }
        let meta = SegmentMeta::of::<R>(KIND_BASELINE);
        Ok(self.write_baseline(&header, meta)?.is_some())
    }

    /// Write one baseline for `header`: close the writer first, then snapshot.
    ///
    /// Closing is the correctness argument, not a tidy-up: the baseline must outrank every
    /// segment already written for the table, and a writer left open would keep appending
    /// to a segment numbered below it, which a replay skips.
    fn write_baseline(&self, header: &[u8], meta: SegmentMeta) -> Result<Option<(String, String)>>
    where
        S: VirtualStorage,
    {
        if let Some(cell) = self.tables.borrow_mut().remove(header) {
            let mut log = cell.into_inner();
            log.seal_last()?;
        }
        let ops = baseline_ops(&self.inner, header)?;
        let cfg = self.cfg.clone();
        write_baseline(&cfg, header, meta, &ops)
    }

    /// Compact the log: one new baseline per table, then delete what it covers.
    ///
    /// A baseline is a checkpoint — the whole truth half at a segment number — so every
    /// segment below it is covered by the snapshot and a replay never needs it. `keep`
    /// decides how many of the newest of those to leave behind (0 keeps none: history
    /// lives in the Delta export, whose versions nothing here can prune).
    ///
    /// Two guards, both about the claim. Compaction is only safe for the holder that may
    /// write: a segment pruned under a successor's feet is a handover that can never
    /// reach its position. And it is refused while a handover is in flight — the position
    /// the holder named must still exist when the successor arrives.
    pub fn compact(&self, keep: usize) -> Result<Vec<(Vec<u8>, Compacted)>>
    where
        S: crate::VirtualStorage,
    {
        let Some(root) = self.cfg.root() else {
            return Ok(Vec::new());
        };
        let headers: Vec<Vec<u8>> = self.metas.borrow().keys().cloned().collect();
        let mut out = Vec::new();
        for header in headers {
            let dir = table_dir(root, &header);
            if let Some(current) = owner::read(&dir).map_err(owner_err)?
                && !self.holds(&current, &dir)
            {
                return Err(Error::Changelog {
                    what: format!(
                        "{}: compaction needs the claim (state {:?}, holder {}); this instance \
                         holds neither",
                        owner::path(&dir).display(),
                        current.state,
                        current.holder
                    ),
                });
            }
            out.push((header.clone(), self.compact_one(&header, keep)?));
        }
        Ok(out)
    }

    /// Compact ONE table. The caller has already settled the claim question.
    fn compact_one(&self, header: &[u8], keep: usize) -> Result<Compacted>
    where
        S: VirtualStorage,
    {
        let Some(root) = self.cfg.root() else {
            return Ok(Compacted {
                baseline: None,
                removed: 0,
                freed: 0,
            });
        };
        let Some(meta) = self.metas.borrow().get(header).cloned() else {
            return Ok(Compacted {
                baseline: None,
                removed: 0,
                freed: 0,
            });
        };
        let Some((first, last)) = self.write_baseline(header, meta.with_kind(KIND_BASELINE))?
        else {
            return Ok(Compacted {
                baseline: None,
                removed: 0,
                freed: 0,
            });
        };
        let (removed, freed) = prune_below(root, header, &first, keep)?;
        let compacted = Compacted {
            baseline: Some(last),
            removed,
            freed,
        };
        Ok(compacted)
    }

    /// Does this instance hold the claim in `current`?
    fn holds(&self, current: &owner::Owner, dir: &Path) -> bool {
        let mine = self.holder.borrow();
        mine.as_deref() == Some(current.holder.as_str())
            && current.state == owner::State::Writing
            && self.leases.borrow().iter().any(|l| l.dir() == dir)
    }

    /// What this table's log owes right now: the open segment, the bytes still
    /// buffered, the records dropped while the sink was broken and the last write
    /// failure. `None` before the first captured write — no writer is open yet.
    pub fn status<R: crate::model::index::Document>(&self) -> Option<ChangelogStatus> {
        let header = R::NS_PREFIX;
        self.tables
            .borrow()
            .get(header)
            .map(|cell| cell.borrow().status())
    }

    /// The headers this commit writes into: registered, and actually present.
    fn touched(&self, ops: &[(Vec<u8>, Option<Vec<u8>>)]) -> Vec<Vec<u8>> {
        let metas = self.metas.borrow();
        let mut out: BTreeSet<Vec<u8>> = BTreeSet::new();
        for (key, _) in ops {
            if let Some(header) = metas.keys().find(|h| key.starts_with(h)) {
                out.insert(header.clone());
            }
        }
        out.into_iter().collect()
    }

    /// Stage this commit's share in every touched table's log. Nothing is written
    /// yet — the frame is memory until the engine says `Ok`.
    fn stage(&self, ops: &[(Vec<u8>, Option<Vec<u8>>)]) -> Vec<Vec<u8>> {
        let touched = self.touched(ops);
        for header in &touched {
            self.with_log(header, |log| {
                log.stage(ops);
            });
        }
        touched
    }

    /// Append what [`Self::stage`] staged — after the commit returned `Ok`.
    ///
    /// A rotation is the only maintenance trigger there is: it is the one moment the writer
    /// is already at a boundary, and the check after it costs a directory read on a rare
    /// event, not a stat per commit.
    fn flush(&self, touched: &[Vec<u8>])
    where
        S: VirtualStorage,
    {
        let mut rotated: Vec<Vec<u8>> = Vec::new();
        for header in touched {
            self.with_log(header, |log| {
                let _ = log.commit_staged();
                if log.took_rotation() {
                    rotated.push(header.clone());
                }
            });
        }
        for header in rotated {
            self.maintain(&header);
        }
    }

    /// Compact this table if its policy says so.
    ///
    /// Best-effort by construction: it runs after a commit already returned `Ok`, so it may
    /// not fail the write it follows. A refusal (someone else holds the claim) is silent —
    /// the successor's own `take_over` is the path that matters, not this one.
    fn maintain(&self, header: &[u8])
    where
        S: VirtualStorage,
    {
        let Some(policy) = self.cfg.compact.clone() else {
            return;
        };
        let Some(root) = self.cfg.root() else {
            return;
        };
        let dir = table_dir(root, header);
        if let Ok(Some(current)) = owner::read(&dir)
            && !self.holds(&current, &dir)
        {
            return;
        }
        let Ok(shape) = log_shape(&dir) else {
            return;
        };
        if shape.uncovered > policy.after_bytes
            || shape
                .baseline_age_secs
                .is_some_and(|age| age >= policy.after_secs)
        {
            let _ = self.compact_one(header, policy.keep);
        }
    }

    /// Run `f` on the table's writer, opening it on first use. A log that cannot
    /// be opened is skipped, never fatal: the write it describes belongs to the
    /// store, and the log is a hint (ADR-0045 §6).
    fn with_log(&self, header: &[u8], f: impl FnOnce(&mut Changelog)) {
        if self.cfg.root().is_none() {
            return;
        }
        if !self.tables.borrow().contains_key(header) {
            let meta = self.metas.borrow().get(header).cloned();
            if let Some(meta) = meta
                && let Ok(mut log) = Changelog::open(self.cfg.clone(), header, meta)
            {
                if let Some(gate) = &self.gate {
                    log.attach_gate(std::sync::Arc::clone(gate));
                }
                self.tables
                    .borrow_mut()
                    .insert(header.to_vec(), RefCell::new(log));
            }
        }
        if let Some(cell) = self.tables.borrow().get(header) {
            f(&mut cell.borrow_mut());
        }
    }
}

impl<S: VirtualStorage> VirtualStorage for ChangelogStore<S> {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        // A point write is a one-op commit: captured like any other, so a host
        // that reaches the engine directly is not invisible either. The clone is
        // the price of the frame's own copy of the bytes.
        let ops = [(key.clone(), Some(value.clone()))];
        let touched = self.stage(&ops);
        self.inner.put(key, value)?;
        self.flush(&touched);
        Ok(())
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn del(&self, key: &[u8]) -> Result<()> {
        let ops = [(key.to_vec(), None)];
        let touched = self.stage(&ops);
        self.inner.del(key)?;
        self.flush(&touched);
        Ok(())
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        self.inner.scan_range(begin, end)
    }

    fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> crate::engine::storage::ScanIter {
        self.inner.scan_range_iter(begin, end)
    }

    fn commit_batch(&self, batch: Batch) -> Result<()> {
        let touched = self.stage(&batch.ops);
        // Only a commit that returned Ok may append: a frame naming a refused
        // write would be a phantom row (ADR-0045 §7).
        self.inner.commit_batch(batch)?;
        self.flush(&touched);
        Ok(())
    }
}
