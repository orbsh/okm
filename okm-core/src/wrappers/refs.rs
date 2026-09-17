//! `Refs<D, K>` — a list of child documents embedded by key
//! reference (ADR-0012 embedded milestone; the many-form of
//! [`Ref`]).
//!
//! Wire form: variable-length cold-segment frame —
//! `[count u32 BE][K::KEY_LEN × count]`. The child documents live at
//! their own keys/ns as complete documents; the parent's field carries
//! only the key sequence.
//!
//! Memory form: `keys: Vec<K>` + `values: Vec<Option<D>>`, **equal
//! length by invariant** — `values[i]` is the dereferenced child of
//! `keys[i]`; `None` = dangling reference (child deleted independently
//! under reference semantics — visible absence, not an error).
//!
//! Key discipline: each child key must carry its own list identity
//! (e.g. `owner_id + seq`) — OKM never appends positional numbers to
//! keys (identity is declared, not manufactured). Keys that differ only
//! by nothing would collapse onto one child.
//!
//! Write semantics (parent `put`'s embed pass):
//! - `values[i] = Some(d)`: child `d` is written under `keys[i]` (the
//!   parent owns this update).
//! - `values[i] = None`: key carried, child untouched (reference).
//! - Stale release: keys pointed at by the OLD parent document but not
//!   the new one are deleted. No cascade — list children are assumed
//!   parent-exclusive; shared children need explicit management.
//!
//! No attribute: recognized from the type, same as `Ref<D, K>`.
//!
//! The dividing line: elements WITH identity (need their own indexes,
//! sharing, independent updates) belong here; pure-value elements
//! (`Vec<String>` fields, dynamic `Array` frames) do NOT — a scalar has
//! no key to reference, so storing it as a key reference is a category
//! error. `Refs` is the declarative form of the one-to-many relation:
//! children live normalized in their own ns, the parent holds the
//! foreign-key set.
use crate::key::KeyEncode;

#[derive(Clone, Debug, PartialEq)]
pub struct Refs<D, K: KeyEncode> {
    /// The child keys in list order (the only thing on the wire).
    pub keys: Vec<K>,
    /// Dereferenced children, index-aligned with `keys`. `None` =
    /// dangling reference on read, or reference-only on write.
    pub values: Vec<Option<D>>,
}

impl<D, K: KeyEncode> Refs<D, K> {
    /// Reference existing children by key (write: no child writes).
    /// Method named `new` — the type name already says "references".
    pub fn new(keys: Vec<K>) -> Self {
        let n = keys.len();
        Refs { keys, values: (0..n).map(|_| None).collect() }
    }

    /// Own all children: write each value under its key on the parent's
    /// put. Panics if `keys.len() != values.len()` — construction bug.
    pub fn own_all(keys: Vec<K>, values: Vec<D>) -> Self {
        assert_eq!(keys.len(), values.len(), "Refs::own_all: keys/values length mismatch");
        Refs { keys, values: values.into_iter().map(Some).collect() }
    }

    /// Mixed form: caller decides per item.
    pub fn from_pairs(pairs: Vec<(K, Option<D>)>) -> Self {
        let keys = pairs.iter().map(|(k, _)| k.clone()).collect();
        let values = pairs.into_iter().map(|(_, v)| v).collect();
        Refs { keys, values }
    }
}
