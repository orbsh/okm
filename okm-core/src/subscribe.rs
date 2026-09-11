//! Subscribe — the channel half of the event layer (ADR-0008).
//!
//! `#[kv_subscribe]` on a `RowEncode` struct declares: this row type's
//! write-path events enter a channel. There is NO handler at the
//! annotation site — the derive only emits a uniform-format send; the
//! processing logic belongs entirely to the consumer, and the combinators
//! (okm-stream, deferred) are the adapter layer.
//!
//! Delivery semantics: best-effort. The send is a synchronous sink call
//! on the write path — with no sink registered events are dropped; with
//! a bounded queue behind the sink a full queue drops rather than
//! blocking the write. No delivery guarantee is offered. Inline
//! consumers (reduce) that need exactly-once MUST NOT ride the channel.
//!
//! The core deliberately knows nothing about tokio (or any executor): a
//! channel is whatever the caller registers via [`ChannelCell::register`]
//! — a tokio mpsc sender, a crossbeam queue, a no-op. Transport is an
//! assembly-site decision, same discipline as engine choice.

/// Operation carried by a subscribe event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Put,
    Delete,
}

/// One channel event — typed payload plus the operation that produced it.
#[derive(Debug)]
pub struct Event<K, R> {
    pub op: Op,
    pub key: K,
    pub row: R,
}

impl<K, R> Event<K, R> {
    pub fn new(op: Op, key: K, row: R) -> Self {
        Self { op, key, row }
    }
}

/// Consumer-agnostic sink: returns whether the event was accepted
/// (`false` = dropped — full queue, no receiver, whatever the transport
/// semantics are). The write path ignores the result.
pub trait EventSink<E>: Send + Sync {
    fn try_send(&self, event: E) -> bool;
}

impl<E, F: Fn(E) -> bool + Send + Sync> EventSink<E> for F {
    fn try_send(&self, event: E) -> bool {
        self(event)
    }
}

/// Global sink cell for one event stream. The caller registers a sink
/// once at assembly time (before or during consumption); emit sites call
/// [`Self::emit`] — no sink registered means events are dropped (the
/// zero-cost default: subscribed rows whose stream nobody consumes pay
/// only one atomic load per write).
pub struct ChannelCell<E> {
    sink: std::sync::RwLock<Option<std::sync::Arc<dyn EventSink<E>>>>,
}

impl<E: 'static> Default for ChannelCell<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: 'static> ChannelCell<E> {
    pub const fn new() -> Self {
        Self {
            sink: std::sync::RwLock::new(None),
        }
    }

    /// Register the transport. Called by the consumer side at assembly
    /// time; a later registration replaces the previous sink (one stream,
    /// one consumer — forking the stream is the caller's move: register a
    /// sink that fans out).
    pub fn register(&self, sink: impl EventSink<E> + 'static) {
        *self.sink.write().unwrap() = Some(std::sync::Arc::new(sink));
    }

    /// Whether a sink is registered (test/inspection use).
    pub fn has_sink(&self) -> bool {
        self.sink.read().unwrap().is_some()
    }

    /// Best-effort send: `false` when no sink is registered or the sink
    /// rejected the event. Never blocks, never panics, never fails the
    /// write that produced the event.
    pub fn emit(&self, event: E) -> bool {
        match &*self.sink.read().unwrap() {
            Some(sink) => sink.try_send(event),
            None => false,
        }
    }
}
