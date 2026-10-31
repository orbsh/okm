//! okm-stream — consumer-side combinators over okm-core's subscribe
//! events (ADR-0008 / ADR-0009). The core's obligation ends at "send a
//! uniform event on the write path, best-effort"; composing those events
//! (filter by field, keep a previous-row cache, fold to an epoch
//! boundary, fan-in several tables) is algorithm-layer work and lives
//! here. Zero storage responsibility: every input is an existing
//! `SinkCell` / generated `SINK_<ENUM>` sink; pull-mode fan-in
//! stays in `okm-query`.
//!
//! Execution model: synchronous. Events are produced by the write path's
//! sync `try_send`, so combinators run inline in that call unless the
//! caller bridges into an executor — the `tokio` feature provides the
//! mpsc adapter for that bridge (core knows no executor; the stream
//! layer's default is the same discipline).

// Tests keep the vocabulary: a panicking test is a failing test with a stack trace
// (and the oracle for every failable path is "assert on the error VALUE"). This line
// is per-crate because `[lints]` is per-package and covers every target, with no way
// to scope a lint to lib/bins — the manifest cannot express `cfg(test)`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]

pub mod ops;

pub use ops::{distinct_by, filter_field, with_previous, Stream, StreamOf};
