//! `okm-value`: the name vocabulary dynamic segments are written in.
//!
//! A segment carries ids rather than field names, so the label ↔ number mapping
//! is part of the value's currency, not of the store's. This crate owns it — the
//! cache, its two directions, and the rules that keep it honest across handles
//! and refused commits — while the caller keeps everything about keys: the door
//! ([`Names`]) is four methods of data in, data out.

pub mod vocab;

pub use vocab::{Id, Names, Vocabulary};