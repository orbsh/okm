//! The object-storage copy of a sealed segment (ADR-0045 §18).
//!
//! What this is: one worker thread and a queue of PATHS. The writer seals a segment,
//! renames it, and tells whoever asked (see [`crate::sink`]); this type is that
//! whoever, and its whole job is to read the finished file and `put` it under
//! `{prefix}/{table hex}/{file name}`.
//!
//! What this deliberately is NOT:
//!
//! * **Not in the commit path.** The hook fires after the rename, from the writer's
//!   thread, and returns as soon as the path is queued. A slow bucket cannot slow a
//!   write, and a bucket outage cannot fail one.
//! * **Not a durability boundary.** The local segment stays the record: a lost upload
//!   is a lost copy, not lost data, and re-running the drain reproduces it.
//!   [`SegmentUploader::failures`] exists so a host can tell the difference instead of
//!   assuming silence means success.
//! * **Not a retry loop that runs forever.** One attempt per job, then the path is
//!   counted and dropped — a queue that retried unboundedly turns a bucket outage into
//!   unbounded memory.
//!
//! The queue holds a `PathBuf` per segment, never the bytes: the file is read at
//! upload time, so a queue of a thousand segments is a queue of a thousand paths.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

use crate::sink::SealedSegmentSink;

/// A job in the queue: what to upload and, for a barrier, where to say it is done.
struct Job {
    table: Vec<u8>,
    name: String,
    path: PathBuf,
    done: Option<oneshot::Sender<()>>,
}

/// Uploads sealed segments to an object store: one worker thread, no policy.
pub struct SegmentUploader {
    /// `None` after shutdown — the sender closing is what ends the worker's loop, so
    /// `Drop` has to be able to take it out before joining.
    tx: Mutex<Option<UnboundedSender<Job>>>,
    failures: Arc<AtomicU64>,
    uploaded: Arc<AtomicU64>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl SegmentUploader {
    /// Start a worker over `store`, putting every segment under `prefix`.
    ///
    /// The store is `Arc<dyn ObjectStore>` rather than a bucket name because the
    /// decision this type makes is "copy the finished file"; WHICH object store that is
    /// (S3, an in-memory store in a test, a filesystem for a local scratch copy) is the
    /// caller's, and testing a copy path needs no credentials.
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Arc<Self> {
        let prefix = prefix.into();
        let (tx, mut rx) = unbounded_channel::<Job>();
        let failures = Arc::new(AtomicU64::new(0));
        let uploaded = Arc::new(AtomicU64::new(0));

        let worker_failures = Arc::clone(&failures);
        let worker_uploaded = Arc::clone(&uploaded);
        let handle = std::thread::Builder::new()
            .name("okm-segment-uploader".into())
            .spawn(move || {
                // A current-thread runtime on its own thread: the writer's thread has no
                // runtime, and giving it one would put `block_on` next to a commit.
                let rt = match tokio::runtime::Builder::new_current_thread().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("okm: segment uploader cannot start a runtime: {e}");
                        return;
                    }
                };
                while let Some(job) = rx.blocking_recv() {
                    // A barrier: everything ahead of it has been attempted.
                    if let Some(done) = job.done {
                        let _ = done.send(());
                        continue;
                    }
                    let key = format!("{}/{}/{}", prefix, hex(&job.table), job.name);
                    let Ok(key) = ObjectPath::parse(&key) else {
                        eprintln!("okm: `{key}` is not a legal object path");
                        worker_failures.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let bytes = match std::fs::read(&job.path) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            eprintln!("okm: {} unreadable at upload: {e}", job.path.display());
                            worker_failures.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    };
                    let put = store.put(&key, PutPayload::from(bytes));
                    match rt.block_on(put) {
                        Ok(_) => {
                            worker_uploaded.fetch_add(1, Ordering::Relaxed);
                        }
                        // One attempt: the local segment is still the record, and a
                        // retry loop here is a bucket outage turned into memory growth.
                        Err(e) => {
                            eprintln!("okm: {key} not copied: {e}");
                            worker_failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
            .ok();

        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            failures,
            uploaded,
            worker: Mutex::new(handle),
        })
    }

    /// Segments copied so far.
    pub fn uploaded(&self) -> u64 {
        self.uploaded.load(Ordering::Relaxed)
    }

    /// Segments this worker could not copy. Not zero-by-assumption: a host that wants to
    /// know whether the copy kept up reads this rather than the absence of noise.
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Wait until everything queued has been attempted, or `timeout` passes.
    ///
    /// A barrier on the queue, for a test or a shutdown — never the write path.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, mut rx) = oneshot::channel();
        if !self.send(Job {
            table: Vec::new(),
            name: String::new(),
            path: PathBuf::new(),
            done: Some(tx),
        }) {
            return false;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(()) => return true,
                Err(oneshot::error::TryRecvError::Closed) => return false,
                Err(oneshot::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        false
    }

    fn send(&self, job: Job) -> bool {
        let Ok(guard) = self.tx.lock() else {
            return false;
        };
        match guard.as_ref() {
            Some(tx) => tx.send(job).is_ok(),
            None => false,
        }
    }
}

impl SealedSegmentSink for SegmentUploader {
    fn sealed(&self, table: &[u8], name: &str, path: &Path) {
        if !self.send(Job {
            table: table.to_vec(),
            name: name.to_string(),
            path: path.to_path_buf(),
            done: None,
        }) {
            // A closed queue is a shut-down worker, not a data problem — but it is a
            // copy that will not happen, so it counts.
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A barrier on the queue: every segment sealed before this point has been
    /// attempted. `false` (timeout, or a closed queue) means the copy is behind the
    /// local log — the truth a cutover must not paper over.
    fn barrier(&self, timeout: std::time::Duration) -> Result<(), String> {
        if self.flush(timeout) {
            Ok(())
        } else {
            Err(format!(
                "segment uploader: queue did not drain within {timeout:?}"
            ))
        }
    }
}
impl Drop for SegmentUploader {
    /// Close the queue and join the worker: a host that drops the uploader does not
    /// leave a thread behind, and the last queued segments are attempted first.
    fn drop(&mut self) {
        if let Ok(mut guard) = self.tx.lock() {
            guard.take();
        }
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
            && std::thread::current().id() != handle.thread().id()
        {
            let _ = handle.join();
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
