//! okm-stream — consumer-side combinators over okm-core's subscribe
//! events (ADR-0008 / ADR-0009). The core's obligation ends at "send a
//! uniform event on the write path, best-effort"; composing those events
//! (filter by field, keep a previous-row cache, fold to an epoch
//! boundary, fan-in several tables) is algorithm-layer work and lives
//! here. Zero storage responsibility: every input is an existing
//! `ChannelCell` / generated `CHANNEL_<ENUM>` sink; pull-mode fan-in
//! stays in `okm-query`.
//!
//! Execution model: synchronous. Events are produced by the write path's
//! sync `try_send`, so combinators run inline in that call unless the
//! caller bridges into an executor — the `tokio` feature provides the
//! mpsc adapter for that bridge (core knows no executor; the stream
//! layer's default is the same discipline).

pub mod ops;

pub use ops::{distinct_by, filter_field, with_previous, Stream, StreamOf};
