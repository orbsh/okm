//! Typed key prefixes: the probe a prefix scan takes (`PrefixProbe<K>`) and the
//! builder a `KeyEncode` derive emits to build one.
//!
//! A prefix probe is a *leading* byte run of a key, so the only legal probes are
//! declaration-order prefixes of the key's fields. Two ways to make that
//! type-safe were considered:
//!
//! - one derived struct per prefix (`X { a, b, c }` -> `X_ab`, `X_a`). Rejected:
//!   it makes the key layout public API (every prefix name is a call-site
//!   name), and inserting a field between two others re-binds every existing
//!   prefix silently — the same name now constrains different bytes, and the
//!   compiler has nothing to say.
//! - a builder whose field methods each exist on the state the previous field
//!   produced (this module). Declaration order becomes a compile-time rule; an
//!   inserted field changes the state type, so the old call stops compiling
//!   instead of changing meaning.
//!
//! The builder itself is emitted per key in the consumer's crate (an inherent
//! `impl` on a foreign generic type is not allowed), and `finish()` is available
//! at every depth — stopping early IS the probe. The empty probe
//! (`K::prefix().finish()`) constrains nothing and scans the table.

use core::marker::PhantomData;

/// A typed prefix probe: the encoded leading fields of key `K`.
///
/// Bytes are opaque to the runtime; what `K` buys is that the builder which
/// produced them was checked against the declaration, and that only
/// `Collection::scan_prefix` on a `Collection<S, K, R>` accepts it.
///
/// The type is deliberately *not* a key: it cannot be used where a `K` is
/// expected, and it never carries a value for the fields it does not name.
pub struct PrefixProbe<K> {
    bytes: Vec<u8>,
    _key: PhantomData<fn() -> K>,
}

impl<K> PrefixProbe<K> {
    /// Wrap encoded leading-field bytes.
    ///
    /// The intended constructor is the derive-emitted builder
    /// (`K::prefix().field(..).finish()`), which guarantees declaration order
    /// and field widths. This door takes the bytes as given — a probe that
    /// does not match the key's layout scans the wrong window, silently.
    #[doc(hidden)]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            _key: PhantomData,
        }
    }

    /// The empty probe: constrains nothing, so every key of the collection
    /// matches and the scan is a full table walk.
    ///
    /// Unlike [`new`](Self::new) this needs no layout knowledge — zero bytes is
    /// a legal prefix of every key — which is why it is a safe constructor
    /// rather than a builder call. It replaces the old implicit "no prefix"
    /// form of `scan_keys` / `scan_document_keys`.
    pub fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            _key: PhantomData,
        }
    }

    /// The encoded prefix bytes (leading key fields, declaration order).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Bytes consumed by the prefix (= sum of the named fields' widths). The
    /// full key is `K::KEY_LEN` bytes, so the last
    /// `K::KEY_LEN - taken()` bytes are unconstrained.
    pub fn taken(&self) -> usize {
        self.bytes.len()
    }
}

impl<K> Clone for PrefixProbe<K> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            _key: PhantomData,
        }
    }
}

impl<K> core::fmt::Debug for PrefixProbe<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PrefixProbe")
            .field("taken", &self.bytes.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}