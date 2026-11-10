//! Who may write a table's log, and where the last writer stopped (ADR-0056 §6).
//!
//! A log has one writer, and on a directory or an object prefix there is nothing that
//! can enforce that: `put` is last-writer-wins and `unlink` is invisible to an open file
//! descriptor. So the claim is not a lock — it is a FACT on the same medium as the
//! segments, and every writer checks it before committing and stops when it is no
//! longer true. That is why this module's job is small: write a file, read it back,
//! and let the reader of the file decide.
//!
//! The file is text, one `key value` per line, unknown keys ignored. Not a byte-packed
//! struct, because its reader is a DIFFERENT BUILD by construction — the old version
//! writes it and the new version reads it during an upgrade — so forward compatibility
//! has to be free, and `cat owner` has to be readable at 3 a.m.
//!
//! ```text
//! state writing
//! epoch 7
//! holder 3f9a1c2e
//! position 00000042.segment
//! ```
//!
//! The state is one word, not a sentence; `position` appears on `handed-over` and names
//! the last segment a READER can see (not the last one the writer sealed — with a sink
//! engaged those differ, because the upload is asynchronous).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The claim's file name, beside the segments it governs.
pub const OWNER: &str = "owner";

/// What the holder is doing. Three words, and the transitions run one way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// May commit. The only state in which a writer writes.
    Writing,
    /// Someone asked for the log; the holder stops accepting work and seals.
    Draining,
    /// Sealed and named: the position is on the file, and the next holder may claim.
    HandedOver,
}

impl State {
    fn word(self) -> &'static str {
        match self {
            State::Writing => "writing",
            State::Draining => "draining",
            State::HandedOver => "handed-over",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "writing" => Some(State::Writing),
            "draining" => Some(State::Draining),
            "handed-over" => Some(State::HandedOver),
            _ => None,
        }
    }
}

/// The file's contents: who holds the log, at which round, in which state, stopped
/// where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub state: State,
    /// Bumped once per takeover, so a late write from the previous holder can be told
    /// from the current one's.
    pub epoch: u64,
    /// Who holds it. A random token per process, not a pid: a pid is reused, and the
    /// claim outlives the process that made it (that is the whole point).
    pub holder: String,
    /// The last segment a reader can see. Set on `handed-over`.
    pub position: Option<String>,
}

impl Owner {
    /// The order the fields are written in. Readers accept any order.
    pub fn render(&self) -> String {
        let mut out = format!(
            "state {}\nepoch {}\nholder {}\n",
            self.state.word(),
            self.epoch,
            self.holder
        );
        if let Some(position) = &self.position {
            out.push_str(&format!("position {position}\n"));
        }
        out
    }

    /// Parse what a holder wrote. A missing or unknown `state` is an ERROR, never a
    /// default: a file this module cannot read is a file whose writer may still be
    /// alive, and guessing would be the split brain the file exists to prevent.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut state = None;
        let mut epoch = None;
        let mut holder = None;
        let mut position = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(' ') else {
                return Err(format!("owner: `{line}` is not a `key value` line"));
            };
            let value = value.trim();
            match key {
                "state" => {
                    state = Some(State::parse(value).ok_or_else(|| {
                        format!("owner: `{value}` is not a state this build knows")
                    })?)
                }
                "epoch" => {
                    epoch = Some(
                        value
                            .parse::<u64>()
                            .map_err(|e| format!("owner: epoch `{value}`: {e}"))?,
                    )
                }
                "holder" => holder = Some(value.to_string()),
                "position" => position = Some(value.to_string()),
                // Forward compatibility is the reason this file is text: a build that
                // writes a field this one does not know must still be readable here.
                _ => {}
            }
        }
        Ok(Self {
            state: state.ok_or("owner: no `state` line")?,
            epoch: epoch.ok_or("owner: no `epoch` line")?,
            holder: holder.ok_or("owner: no `holder` line")?,
            position,
        })
    }
}

/// The claim's path inside a table directory.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(OWNER)
}

/// Read the claim, if there is one. `Ok(None)` is "nobody has claimed this directory",
/// which is the normal state of a fresh table.
pub fn read(dir: &Path) -> Result<Option<Owner>, String> {
    let path = path(dir);
    match fs::read_to_string(&path) {
        Ok(text) => Owner::parse(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Replace the claim atomically: a temporary file, fsync, rename, fsync the directory.
///
/// The same primitive a segment is sealed with, and for the same reason: a reader must
/// see the previous claim or the next one, never half of either. Not a delete-then-create
/// — `unlink` is invisible to an open descriptor on POSIX and has no atomic form on an
/// object store.
pub fn write(dir: &Path, owner: &Owner) -> Result<(), String> {
    let path = path(dir);
    let temp = dir.join(format!(".{OWNER}.{}.tmp", std::process::id()));
    let bytes = owner.render();
    {
        let mut file = fs::File::create(&temp).map_err(|e| format!("{}: {e}", temp.display()))?;
        file.write_all(bytes.as_bytes())
            .map_err(|e| format!("{}: {e}", temp.display()))?;
        file.sync_all()
            .map_err(|e| format!("{}: {e}", temp.display()))?;
    }
    fs::rename(&temp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Durable rename: without this a crash can leave the previous claim in place while
    // the segments it describes have moved on.
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// Ask the current holder to stop (the new build's first write, ADR-0056 §6).
///
/// The epoch is bumped so the answer is unambiguous: a `handed-over` carrying this
/// epoch is the reply to THIS request, not to one an earlier attempt made.
pub fn announce_drain(dir: &Path, holder: &str) -> Result<Owner, String> {
    let epoch = read(dir)?.map(|owner| owner.epoch).unwrap_or(0) + 1;
    let owner = Owner {
        state: State::Draining,
        epoch,
        holder: holder.to_string(),
        position: None,
    };
    write(dir, &owner)?;
    Ok(owner)
}

/// The old holder's last write: sealed, and the position a reader can see.
pub fn hand_over(dir: &Path, epoch: u64, holder: &str, position: &str) -> Result<(), String> {
    write(
        dir,
        &Owner {
            state: State::HandedOver,
            epoch,
            holder: holder.to_string(),
            position: Some(position.to_string()),
        },
    )
}

/// Wait for the holder of `epoch` to finish handing over, and answer the position it
/// named. `Ok(None)` means the timeout passed — the caller must NOT claim on it.
pub fn wait_for_handover(
    dir: &Path,
    epoch: u64,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(owner) = read(dir)?
            && owner.state == State::HandedOver
            && owner.epoch == epoch
        {
            return Ok(owner.position);
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A random-per-process token. Not a pid: pids are reused, and a claim outlives the
/// process that made it.
pub fn fresh_holder() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("{:016x}", hasher.finish())
}

/// Timings for the poll loop: how often the file is re-read, and how often the claim is
/// refreshed. Defaults are the ADR's; a test wants them short.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    pub tick: Duration,
    pub heartbeat: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(500),
            heartbeat: Duration::from_secs(3),
        }
    }
}

/// A held claim: the file, the poll loop that keeps it true, and the switch the commit
/// path reads.
///
/// The switch is an `AtomicBool` and never a syscall, because it is read at every
/// commit. It goes `false` when the poller finds the claim is no longer ours — the file
/// says another holder, another epoch, or a later state — and it stays false: a writer
/// that lost the log does not get it back by waiting.
pub struct Lease {
    dir: PathBuf,
    owner: Owner,
    may_write: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Lease {
    /// Claim a table's log and start the poll loop.
    ///
    /// Refuses when the file already says someone else is `writing`: taking over is an
    /// explicit act ([`announce_drain`] then [`hand_over`]), never a side effect of
    /// opening a log. A stale claim is indistinguishable from a live one from here, and
    /// guessing is what the file exists to prevent.
    pub fn claim(dir: impl Into<PathBuf>, holder: &str) -> Result<Arc<Self>, String> {
        Self::claim_with(dir, holder, Timings::default())
    }

    /// [`Lease::claim`] with explicit timings, for tests and for a host that wants a
    /// slower poll on a network filesystem.
    pub fn claim_with(
        dir: impl Into<PathBuf>,
        holder: &str,
        timings: Timings,
    ) -> Result<Arc<Self>, String> {
        let dir = dir.into();
        let previous = read(&dir)?;
        if let Some(owner) = &previous
            && owner.state == State::Writing
            && owner.holder != holder
        {
            return Err(format!(
                "{}: held by `{}` at epoch {} — take over, do not claim",
                path(&dir).display(),
                owner.holder,
                owner.epoch
            ));
        }
        let owner = Owner {
            state: State::Writing,
            epoch: previous.map(|owner| owner.epoch).unwrap_or(0) + 1,
            holder: holder.to_string(),
            position: None,
        };
        write(&dir, &owner)?;

        let may_write = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let lease = Arc::new(Self {
            dir: dir.clone(),
            owner: owner.clone(),
            may_write: Arc::clone(&may_write),
            stop: Arc::clone(&stop),
            worker: Mutex::new(None),
        });

        let poll_dir = dir.clone();
        let poll_owner = owner.clone();
        let handle = std::thread::Builder::new()
            .name("okm-owner-lease".into())
            .spawn(move || {
                let mut last_refresh = Instant::now();
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(timings.tick);
                    match read(&poll_dir) {
                        Ok(Some(current))
                            if current.state == State::Writing
                                && current.holder == poll_owner.holder
                                && current.epoch == poll_owner.epoch => {}
                        // Not ours any more: another holder, another round, or a state
                        // past writing. Stop committing and stay stopped.
                        Ok(_) => {
                            may_write.store(false, Ordering::SeqCst);
                            return;
                        }
                        // An unreadable file is not evidence of ownership either.
                        Err(_) => {
                            may_write.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                    if last_refresh.elapsed() >= timings.heartbeat {
                        // The lease is only as good as the last successful write: a
                        // holder that cannot renew must stop, or a partition becomes
                        // two writers.
                        if write(&poll_dir, &poll_owner).is_err() {
                            may_write.store(false, Ordering::SeqCst);
                            return;
                        }
                        last_refresh = Instant::now();
                    }
                }
            })
            .ok();
        if let Ok(mut guard) = lease.worker.lock() {
            *guard = handle;
        }
        Ok(lease)
    }

    /// The switch the commit path reads: `true` while this writer may still write.
    pub fn gate(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.may_write)
    }

    pub fn may_write(&self) -> bool {
        self.may_write.load(Ordering::SeqCst)
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }

    /// Where the claim lives, so a host can log it without re-deriving the path.
    pub fn path(&self) -> PathBuf {
        path(&self.dir)
    }

    /// Stop the poll loop and leave the file as it is: a shut-down writer that never
    /// drained leaves a claim behind, and the next holder must take over explicitly
    /// rather than find a cleared directory.
    pub fn release(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
            && std::thread::current().id() != handle.thread().id()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release();
    }
}