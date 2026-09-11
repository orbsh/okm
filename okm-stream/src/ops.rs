//! Stream combinators. A [`Stream`] is a boxed consumer: a sink that
//! receives events and pushes derived results to its own downstream.
//! Combinators wrap one stream in another, so chains build push-mode
//! pipelines: `filter_field(...).with_previous(...)` attached to a
//! generated `CHANNEL_<ENUM>` via `register`.
//!
//! Everything here is best-effort by inheritance: the write path's
//! `try_send` contract passes through unchanged — a downstream that
//! rejects (`false`) makes the whole chain reject, and the event is
//! dropped at the source. No buffering, no retry: precision-bound
//! consumers belong inline (reduce), not on a channel.

use std::collections::HashMap;
use std::hash::Hash;

use okm_core::subscribe::Event;

/// A push-mode event consumer. `false` = this stage rejected the event
/// (and the source drops it). `Send + Sync + 'static` so a stream can be
/// registered directly as a [`okm_core::subscribe::EventSink`].
pub type StreamOf<E> = Box<dyn FnMut(E) -> bool + Send + Sync + 'static>;

/// The stream handle: wraps a [`StreamOf`] and implements
/// [`okm_core::subscribe::EventSink`], so `cell.register(stream)` just
/// works.
/// Shared mutability is structural, not incidental: `EventSink` is
/// `&self` (the write path holds only a shared reference), so every
/// stateful stage wraps its `FnMut` in a Mutex. Chains are built at
/// assembly time and driven single-threaded per emit — the lock is
/// uncontended on the hot path.
type SharedSink<E> = std::sync::Arc<std::sync::Mutex<StreamOf<E>>>;

pub struct Stream<E> {
    sink: SharedSink<E>,
}

impl<E: 'static> Stream<E> {
    pub fn new(sink: impl FnMut(E) -> bool + Send + Sync + 'static) -> Self {
        Self { sink: std::sync::Arc::new(std::sync::Mutex::new(Box::new(sink))) }
    }
}

impl<E: 'static> okm_core::subscribe::EventSink<E> for Stream<E> {
    fn try_send(&self, event: E) -> bool {
        (self.sink.lock().unwrap())(event)
    }
}

impl<E: 'static> Stream<E> {
    /// Pass events through where `pred` holds (field-level subscription
    /// lives here: `filter_field` on the row inside the event).
    pub fn filter(self, pred: impl Fn(&E) -> bool + Send + Sync + 'static) -> Self {
        let inner = self.sink;
        Self::new(move |e| {
            if pred(&e) {
                (inner.lock().unwrap())(e)
            } else {
                true // filtered out = delivered-to-nothing, not rejected
            }
        })
    }

}

impl<E: 'static> Stream<E> {
    /// Transform events flowing through the pipeline: the returned
    /// stream receives `E2`, applies `f`, forwards `E` to this stage's
    /// downstream. (Push pipelines compose inside-out — the existing
    /// sink is downstream, the returned stream is what registers at the
    /// channel cell.)
    pub fn map<E2: 'static>(self, f: impl Fn(E2) -> E + Send + Sync + 'static) -> Stream<E2> {
        let inner = self.sink;
        Stream::<E2>::new(move |e| (inner.lock().unwrap())(f(e)))
    }
}

/// Field-level subscription: keep only events whose row passes `pred`.
/// The declaration-side feature does not exist by decision (see PLAN):
/// interest sets belong to consumers, one per consumer, no second source
/// of truth.
pub fn filter_field<K, R>(
    pred: impl Fn(&R) -> bool + Send + Sync + Clone + 'static,
) -> impl Fn(Stream<Event<K, R>>) -> Stream<Event<K, R>>
where
    K: 'static,
    R: 'static,
{
    move |stream| {
        let pred = pred.clone();
        stream.filter(move |ev| pred(&ev.row))
    }
}

/// Previous-row cache keyed by the event key: replaces each event's
/// `row` with `(old, new)` and hands the pair downstream. The consumer
/// derives before/after (diff, change detection) at zero write-path
/// cost — this is the combinator that replaced `Event.old` (evaluated
/// and rejected: see PLAN).
///
/// Note: the cache reflects what this consumer has *seen*, not what the
/// store holds — dropped events (best-effort) desync it until the next
/// put re-primes the entry. That is the channel contract, not a defect.
#[allow(clippy::type_complexity)] // the pair type IS the combinator's contract
pub fn with_previous<K, R>(
) -> impl Fn(Stream<Event<K, (Option<R>, R)>>) -> Stream<Event<K, R>>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    move |stream| {
        let cache: std::sync::Arc<std::sync::Mutex<HashMap<K, R>>> = Default::default();
        stream.map::<Event<K, R>>(move |ev| {
            let old = cache.lock().unwrap().insert(ev.key.clone(), ev.row.clone());
            Event::<K, (Option<R>, R)>::new(ev.op, ev.epoch, ev.key, (old, ev.row))
        })
    }
}

/// Change detection on top of [`with_previous`]: pass only events where
/// `changed(&old, &new)` holds (first sight of a key counts as changed).
#[allow(clippy::type_complexity)] // ditto
pub fn distinct_by<K, R>(
    changed: impl Fn(&R, &R) -> bool + Send + Sync + Clone + 'static,
) -> impl Fn(Stream<Event<K, (bool, R)>>) -> Stream<Event<K, R>>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    move |stream| {
        let cache: std::sync::Arc<std::sync::Mutex<HashMap<K, R>>> = Default::default();
        let changed = changed.clone();
        stream.map::<Event<K, R>>(move |ev| {
            let old = cache.lock().unwrap().insert(ev.key.clone(), ev.row.clone());
            let is_changed = match old.as_ref() {
                Some(o) => changed(o, &ev.row),
                None => true,
            };
            Event::<K, (bool, R)>::new(ev.op, ev.epoch, ev.key, (is_changed, ev.row))
        })
    }
}
