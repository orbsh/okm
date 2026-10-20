//! `Vocab<T>` — an interned categorical value (ADR-0032).
//!
//! A categorical field is a **label**, not a number: the name is the value, the
//! id is an encoding detail interned in that FIELD's own data vocabulary
//! ([`crate::model::obj_vocab::data_vocab`], counting family 4, one pair of
//! faces per declared `Vocab` field). The in-memory form carries the name; the
//! wire form is one byte, so a `Vocab` field stays hot-segment eligible at
//! exactly the cost the retired `Enum<T>` had.
//!
//! `T` is a zero-size domain marker: `Vocab<Status>` and `Vocab<Color>`
//! are different types, so a colour cannot be assigned to a status field. It
//! carries no runtime data and needs no trait — a plain unit type does the job.
//!
//! ## Lifecycle
//!
//! - **Build**: [`Vocab::of`] takes the name; the id is unknown until interning.
//! - **Intern**: a write-path step (the `Intern` hook in the derive) resolves
//!   name → id against the collection's vocabulary. Encoding an uninterned
//!   value panics rather than writing a plausible-but-wrong byte.
//! - **Decode**: the wire gives the id; [`Vocab::from_id`] carries it without a
//!   name, and the read path's deref step fills the name from the vocabulary —
//!   the same shape the embedded-reference hook uses.
//!
//! There is deliberately no ordering: categorical strings have none, and the
//! vocabulary's ids are arrival order. A field that must sort semantically
//! declares a separate rank column (ADR-0032 §6).

use std::fmt;
use std::marker::PhantomData;

/// Highest id a `Vocab` field can carry: the wire is one byte. Interning the
/// 256th distinct value in one collection is a loud error, not a wrap
/// (ADR-0032 §3).
pub const MAX_VOCAB_ID: u16 = 255;

/// An interned categorical value: name in memory, one-byte id on the wire.
///
/// `T` names the domain and never appears in the encoding.
pub struct Vocab<T> {
    /// The label. `None` = decoded from an id and not yet resolved (the deref
    /// step fills it); `Some("")` is a legal, distinct label — not a sentinel.
    name: Option<String>,
    /// The interned id. `None` = built from a name, not yet interned.
    id: Option<u8>,
    _domain: PhantomData<T>,
}

impl<T> Vocab<T> {
    /// Build from a name — the normal construction for a write. The value is
    /// not encodable until the write path interns it.
    pub fn of(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            id: None,
            _domain: PhantomData,
        }
    }

    /// Reconstruct from the wire: the id is known, the name is not.
    pub fn from_id(id: u8) -> Self {
        Self {
            name: None,
            id: Some(id),
            _domain: PhantomData,
        }
    }

    /// The label, when this value has one (built from a name, or resolved
    /// after a decode).
    pub fn as_str(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The interned id, when this value has one.
    pub fn id(&self) -> Option<u8> {
        self.id
    }

    /// Record the id the vocabulary handed out. Called by the intern hook.
    pub fn set_id(&mut self, id: u8) {
        self.id = Some(id);
    }

    /// Fill the name after a decode. Called by the deref hook; a name already
    /// present is never overwritten (the constructor's label wins).
    pub fn set_name(&mut self, name: impl Into<String>) {
        if self.name.is_none() {
            self.name = Some(name.into());
        }
    }

    /// The one-byte wire form. Panics when the value was never interned —
    /// a missing id is a write-path bug (the derive's intern step is what
    /// prevents it), never something to paper over with a default byte.
    pub fn to_wire(&self) -> [u8; 1] {
        match self.id {
            Some(id) => [id],
            None => panic!(
                "Vocab::to_wire: `{}` was never interned — a Vocab field must go through \
                 Collection::put / save_with_prev_into (which intern before encoding), not \
                 through an encoding-only path with a hand-built document",
                self.name.as_deref().unwrap_or("<unnamed>")
            ),
        }
    }

    /// Inverse of [`to_wire`](Self::to_wire).
    pub fn from_wire(b: &[u8]) -> Self {
        assert!(b.len() == 1, "Vocab wire is exactly one byte");
        Self::from_id(b[0])
    }
}

impl<T> Clone for Vocab<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            id: self.id,
            _domain: PhantomData,
        }
    }
}

impl<T> Default for Vocab<T> {
    /// The empty label, uninterned. A default `Vocab` encodes only after the
    /// intern step resolves it, exactly like any other built value.
    fn default() -> Self {
        Self::of(String::new())
    }
}

/// Equality is by LABEL when both sides carry one — that is what business code
/// means by "the same category". Two decoded values without names compare by
/// id, and a named value never equals a nameless one (they are not the same
/// fact: one is resolved, the other is not).
impl<T> PartialEq for Vocab<T> {
    fn eq(&self, other: &Self) -> bool {
        match (&self.name, &other.name) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.id == other.id,
            _ => false,
        }
    }
}

impl<T> Eq for Vocab<T> {}

impl<T> fmt::Debug for Vocab<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.name, self.id) {
            (Some(n), Some(i)) => write!(f, "Vocab({n:?} = {i})"),
            (Some(n), None) => write!(f, "Vocab({n:?}, uninterned)"),
            (None, Some(i)) => write!(f, "Vocab(id {i}, unresolved)"),
            (None, None) => write!(f, "Vocab(unresolved)"),
        }
    }
}

impl<T> From<&str> for Vocab<T> {
    fn from(s: &str) -> Self {
        Self::of(s)
    }
}

impl<T> From<String> for Vocab<T> {
    fn from(s: String) -> Self {
        Self::of(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Status;

    #[test]
    fn name_is_the_value_and_id_is_the_encoding() {
        let mut v: Vocab<Status> = Vocab::of("active");
        assert_eq!(v.as_str(), Some("active"));
        assert_eq!(v.id(), None);
        v.set_id(7);
        assert_eq!(v.to_wire(), [7]);
        assert_eq!(v.id(), Some(7));
        // Round trip through the wire loses the name until deref fills it.
        let back = Vocab::<Status>::from_wire(&v.to_wire());
        assert_eq!(back.as_str(), None);
        assert_eq!(back.id(), Some(7));
    }

    #[test]
    fn equality_follows_the_label() {
        let mut a: Vocab<Status> = Vocab::of("active");
        let mut b: Vocab<Status> = Vocab::of("active");
        a.set_id(0);
        b.set_id(3);
        assert_eq!(a, b, "same label, different ids — same category");
        let c: Vocab<Status> = Vocab::of("closed");
        assert_ne!(a, c);
        // Resolved vs unresolved never compares equal: they are different facts.
        let d: Vocab<Status> = Vocab::from_id(0);
        assert_ne!(a, d);
    }

    #[test]
    fn deref_fills_the_name_without_overwriting_a_built_one() {
        let mut decoded: Vocab<Status> = Vocab::from_id(2);
        decoded.set_name("suspended");
        assert_eq!(decoded.as_str(), Some("suspended"));
        decoded.set_name("ignored");
        assert_eq!(decoded.as_str(), Some("suspended"));
    }

    #[test]
    #[should_panic(expected = "was never interned")]
    fn encoding_an_uninterned_value_is_loud() {
        let v: Vocab<Status> = Vocab::of("active");
        let _ = v.to_wire();
    }

    #[test]
    fn empty_label_is_a_label_not_a_sentinel() {
        let v: Vocab<Status> = Vocab::of("");
        assert_eq!(v.as_str(), Some(""));
        assert_ne!(v, Vocab::<Status>::of("x"));
    }
}