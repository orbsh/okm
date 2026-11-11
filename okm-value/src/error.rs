//! The codec's own failures.
//!
//! One variant is the segment's (a value too wide for its 16-bit table), the
//! other is the caller's, carried out unchanged: a name resolution failure is
//! the writer's error type — an engine refusal, a full vocabulary — and this
//! crate must not flatten it into a string on the way out.

use std::fmt;

/// A failure on the encode side. `decode` has none: bad bytes are an empty
/// field list, not an error (ADR-0040 — data that cannot be decoded is data
/// that is not there).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// The value cannot be represented: the block table's length no longer fits
    /// the head's sixteen bits.
    Corrupt { what: String },
    /// The caller's name resolver refused — reported as it handed it over.
    Names(E),
}

impl<E> From<E> for Error<E> {
    /// The caller's failure becomes this crate's, without changing shape —
    /// `?` on a resolver call is the whole conversion.
    fn from(error: E) -> Self {
        Error::Names(error)
    }
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Corrupt { what } => write!(f, "dynamic segment: {what}"),
            Error::Names(e) => write!(f, "name resolution: {e}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Corrupt { .. } => None,
            Error::Names(e) => Some(e),
        }
    }
}
