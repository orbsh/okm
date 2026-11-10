//! Where a sealed segment goes besides the disk (ADR-0045 §18, ADR-0056 §7).
//!
//! The capture's contract with a host is one hook and no policy: the writer tells
//! whoever asked that a segment has been SEALED — the rename has happened, the file
//! is complete and every reader can see it — and the host decides what that means.
//! The writer never waits for it, never uploads anything itself, and never lets the
//! hook run inside a commit: a segment becomes readable at seal, so that is the
//! earliest moment a second copy can be made without copying a file that is still
//! growing.
//!
//! Implementations must be cheap and must not panic: `sealed` is called from the
//! writer's own thread, between commits.

use std::path::Path;
use std::sync::Arc;

/// A consumer of sealed segments.
///
/// `table` is the table's header (the ns bytes) and `path` is the sealed file —
/// both handed over so an implementation needs no knowledge of the log's layout.
pub trait SealedSegmentSink: Send + Sync {
    /// A segment was sealed and renamed into place.
    fn sealed(&self, table: &[u8], name: &str, path: &Path);
}

/// The sink a log has when nobody asked for one: it does nothing, and costs a branch.
pub struct NoSink;

impl SealedSegmentSink for NoSink {
    fn sealed(&self, _table: &[u8], _name: &str, _path: &Path) {}
}

/// Take a sink as the writer's own type, or none.
pub type SharedSink = Option<Arc<dyn SealedSegmentSink>>;
