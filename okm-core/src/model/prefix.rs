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

/// A typed prefix probe for access method `I`: the encoded leading fields of
/// an index's declared `fields(...)`, in declaration order.
///
/// The mirror of [`PrefixProbe`] on the other side of the model. The index
/// marker type pins WHICH access method the bytes belong to, so a probe built
/// for `by_org` cannot be spent on `by_label`; the bytes stay opaque because the
/// fields are payload fields (any payload encoding may appear in them, including
/// variable width) rather than a fixed-width key.
pub struct IndexProbe<I> {
    bytes: Vec<u8>,
    _index: PhantomData<fn() -> I>,
}

impl<I> IndexProbe<I> {
    /// Wrap encoded leading-field bytes.
    ///
    /// The intended constructor is the derive-emitted builder
    /// (`__OkmIndex_User_by_org::prefix().org_id(7).finish()`); this door takes
    /// the bytes as given, for probes assembled from data.
    #[doc(hidden)]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            _index: PhantomData,
        }
    }

    /// The empty probe: constrains no field, so the whole access method
    /// matches (every entry, in entry order).
    pub fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            _index: PhantomData,
        }
    }

    /// The encoded prefix bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// True for the empty probe.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl<I> Clone for IndexProbe<I> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            _index: PhantomData,
        }
    }
}

impl<I> core::fmt::Debug for IndexProbe<I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IndexProbe")
            .field("bytes", &self.bytes)
            .finish()
    }
}

/// What a scan door accepts as a probe: a typed [`IndexProbe`] for the index it
/// names, or raw encoded bytes for callers that assemble them from data.
///
/// The raw arms are the escape hatch `PrefixProbe::new` is on the key side; the
/// typed arm is what the derive-emitted builder produces, and it is the one that
/// cannot be spent on the wrong access method.
pub trait ProbeBytes<I> {
    fn probe_bytes(&self) -> &[u8];
}

impl<I> ProbeBytes<I> for IndexProbe<I> {
    fn probe_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl<I> ProbeBytes<I> for &IndexProbe<I> {
    fn probe_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl<I> ProbeBytes<I> for &[u8] {
    fn probe_bytes(&self) -> &[u8] {
        self
    }
}

impl<I> ProbeBytes<I> for Vec<u8> {
    fn probe_bytes(&self) -> &[u8] {
        self
    }
}

impl<I> ProbeBytes<I> for &Vec<u8> {
    fn probe_bytes(&self) -> &[u8] {
        self
    }
}

impl<I, const N: usize> ProbeBytes<I> for &[u8; N] {
    fn probe_bytes(&self) -> &[u8] {
        *self
    }
}