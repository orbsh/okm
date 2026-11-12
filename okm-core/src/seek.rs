//! The repositioning contract: an ordered stream that can be moved forward to a
//! bound instead of stepped through every key in between.
//!
//! [`Iterator`] states the minimum a consumer needs — one item at a time — and
//! that minimum is why aligning two streams (`okm-query`'s `intersect`) pays one
//! `next()` per key the trailing side has to pass. A SCAN can do better: the
//! engine holds a cursor, and fjall, redb, the remote paged stream and the
//! buffered arms all answer [`ScanIter::seek`](crate::engine::storage::ScanIter::seek)
//! — "move to the first entry at or after `lower`" — so a consumer that knows
//! where it wants to be can jump there in one reposition instead of a thousand
//! steps.
//!
//! [`Seekable`] is that capability as a trait, kept here (not in `okm-query`)
//! because the ability belongs to the engine side: `okm-query` depends on this
//! crate, so the contract lives where the primitive does and the operator that
//! uses it lives one crate up.
//!
//! It is deliberately NOT an [`Iterator`] extension: an iterator's position is
//! invisible (peeking costs a clone or a `Peekable`), while a repositioning
//! consumer needs to read the position, compare it against the other side, and
//! only then move. [`key`](Seekable::key) is the position, and it is the same
//! item the next [`step`](Seekable::step) consumes — a seek never eats what it
//! lands on.

/// An ordered stream that can be repositioned FORWARD to a lower bound.
///
/// The three verbs are one cursor:
///
/// - [`key`](Seekable::key) — the item on top, `None` once the stream is spent.
///   Reading it does not consume it.
/// - [`step`](Seekable::step) — advance one item, answering whether one is
///   there. Exactly the inner iterator's `next()` succeeding: same order, same
///   set, no skipping and no reordering.
/// - [`seek`](Seekable::seek) — move to the first item whose key is `>= lower`,
///   answering whether anything is left.
///
/// # Contract
///
/// - **Ordered.** Items ascend by [`Key`](Seekable::Key). Every seek lower bound
///   and every comparison between two streams assumes it; an unordered stream
///   does not error, it silently misses keys (the same contract `intersect`
///   states over plain iterators).
/// - **Forward only.** A `lower` at or BEHIND the current position is a no-op
///   that keeps the position — a seek is a cursor move, not a re-query. A caller
///   walking ascending targets therefore never pays to go back, and one that
///   guesses too high keeps what it has instead of re-reading items it already
///   consumed. A seek never moves backwards, even to an item the stream has
///   already handed out.
/// - **`false` means nothing at or after `lower`.** Not "an error", not "try
///   again": the stream is spent, and `key()` stays `None`.
/// - **Seek does not consume.** The item a seek lands on is still on top: `key()`
///   shows it, `step()` hands it out (and returns `true`). This is what lets a
///   consumer seek to a target, compare, and only then decide.
/// - **A seek is a position, not a read.** On an engine whose cursor can be
///   reopened (fjall's snapshot, redb's read transaction, the remote pager) the
///   skipped entries are never pulled, so they cost nothing but the reposition.
///   On an arm with no cursor (a buffered test map, a type-erased external
///   source) `seek` degrades to draining the gap — same answers, the skip is
///   not free there.
///
/// # Cost, and why an operator would care
///
/// A step is a cursor increment: nanoseconds, the entry already in hand. A seek
/// is a reposition — on fjall and redb it reopens the range on the snapshot /
/// transaction the scan already holds, i.e. microseconds, tens of steps' worth.
/// So seeking is worth it for a GAP, not for an adjacent key: a consumer that
/// chases the other stream should step over a few entries and reposition only
/// past a run. `okm-query`'s `intersect_seek` states that trade as a budget.
///
/// There is no error channel here on purpose: the verbs answer positions
/// (`bool`, `Option<&Key>`), and an engine fault is a value, not a position. An
/// adapter over a fallible source reports a fault out of band (see
/// [`SeekScan::error`](crate::model::document::SeekScan::error)) rather than
/// pretending the stream ended cleanly.
pub trait Seekable {
    /// What the stream is ordered by, and the bound a seek accepts.
    type Key: Ord;

    /// The item on top — the current position — or `None` when the stream is
    /// spent. Borrowing, so comparing two positions costs no clone.
    fn key(&self) -> Option<&Self::Key>;

    /// Advance one item. `false` when there was nothing left to advance to,
    /// after which [`key`](Self::key) stays `None`.
    fn step(&mut self) -> bool;

    /// Move forward to the first item `>= lower`, without consuming it.
    ///
    /// `false` when the stream has nothing at or after `lower`; a `lower` at or
    /// behind the current position is a no-op that keeps the position and
    /// answers whether the stream is still live.
    fn seek(&mut self, lower: &Self::Key) -> bool;
}
