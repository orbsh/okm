//! `Ref<D, K>` — a child document embedded into a parent document
//! by key reference (ADR-0012 embedded-type milestone).
//!
//! Wire form: **key only** (`K::encode()`, fixed width — hot-segment
//! friendly). The child document lives at its own key/ns as a complete
//! document with its own indexes; the parent's field carries just the
//! pointer. Memory form: `key` + `Option<value>` —
//!
//! - write, `Some(d)`: the child document is written/overwritten by the
//!   parent's put (the parent owns this update).
//! - write, `None`: reference an already-existing child — the parent
//!   stores the key and touches nothing else (shared, many-to-one).
//! - read: `Collection::get` dereferences — the child is fetched by the
//!   stored key and backfilled as `Some(d)`; a missing child stays
//!   `None` (visible absence, not a panic — under reference semantics
//!   the child can be deleted independently).
//!
//! No attribute: `Ref<D, K>` in field position is recognized by the
//! derive from the type itself, same discipline as `Reverse<T>` /
//! `VarInt<T>` / `Quant<P>`.
use crate::key::KeyEncode;

#[derive(Clone, Debug, PartialEq)]
pub struct Ref<D, K: KeyEncode> {
    /// The child document's own key (the only thing on the wire).
    pub key: K,
    /// The child document in memory. `Some` = write it / it was read
    /// back; `None` = reference-only write, or child missing on read.
    pub value: Option<D>,
}

impl<D, K: KeyEncode> Ref<D, K> {
    /// Reference an existing child by key (write: no child write).
    pub fn ref_key(key: K) -> Self {
        Ref { key, value: None }
    }

    /// Alias of `ref_key` — construct from the key alone (used by the
    /// junction derive's `from_parts`).
    pub fn from_key(key: K) -> Self {
        Ref { key, value: None }
    }

    /// Own a child document: write it under this key on the parent's put.
    pub fn own(key: K, value: D) -> Self {
        Ref { key, value: Some(value) }
    }

    /// Wire bytes = the child key's encoding.
    pub fn encode_key(&self) -> Vec<u8> {
        self.key.encode()
    }
}
