//! `Vocab<N>` — an interned categorical label (ADR-0032).
//!
//! A categorical field is a **label**, not a number: the string is the value,
//! the id is an encoding detail interned in that FIELD's own data vocabulary
//! ([`crate::model::obj_vocab::data_vocab`]: two counting-family faces, one
//! counter per declared field).
//!
//! `N` is the **id width**, and it is the only knob: `Vocab<u8>` (the default)
//! spends one byte per row on up to 256 distinct labels, `Vocab<u16>` two bytes
//! on up to 65536, and so on through `u128`. That is the whole trade — label
//! count against payload width — so the declaration states it directly.
//!
//! There is no other categorical form: a numeric code is a number field, and
//! enum semantics (match arms, exhaustiveness) belong to the consumer, which
//! wraps the field in its own Rust enum and converts at the edge.
//!
//! ## Lifecycle
//!
//! - **Build**: [`Vocab::of`] takes the label; the id is unknown until interning.
//! - **Intern**: a write-path step (the derive's intern hook) resolves
//!   label → id against the field's vocabulary. Encoding an uninterned value
//!   panics rather than writing a plausible-but-wrong id.
//! - **Decode**: the wire gives the id; [`Vocab::from_id`] carries it without a
//!   label, and the read path's deref step fills it from the vocabulary.
//!
//! There is deliberately no ordering: labels have none, and the vocabulary's
//! ids are arrival order. A field that must sort semantically declares a
//! separate rank column (ADR-0032 §6).

use std::fmt;
use std::marker::PhantomData;

/// The id width a `Vocab` field spends: big-endian so byte order stays value
/// order (an index over the id scans in allocation order).
pub trait VocabId: Copy + Ord + Eq + std::hash::Hash + fmt::Debug + 'static {
    /// Bytes on the wire (1 for `u8` … 16 for `u128`).
    const WIDTH: usize;
    /// The lowest id — every vocabulary starts allocating here.
    const ZERO: Self;
    /// Checked increment: `None` when the vocabulary is full.
    fn succ(self) -> Option<Self>;
    /// Big-endian wire form, exactly [`VocabId::WIDTH`] bytes.
    fn to_be(self) -> Vec<u8>;
    /// The big-endian image, right-aligned in a fixed 16-byte array so the last
    /// [`VocabId::WIDTH`] bytes ARE the wire form. No allocation, and the
    /// generated encoders can slice it for any width (`&img[16 - W..]`).
    fn be16(self) -> [u8; 16];
    /// Inverse of [`VocabId::to_be`]; `None` on a length or range mismatch.
    fn from_be(b: &[u8]) -> Option<Self>;
    /// For messages and tests.
    fn as_u128(self) -> u128;
}

macro_rules! vocab_id {
    ($($t:ty => $w:literal),+ $(,)?) => {$(
        impl VocabId for $t {
            const WIDTH: usize = $w;
            const ZERO: Self = 0;
            fn succ(self) -> Option<Self> {
                self.checked_add(1)
            }
            fn to_be(self) -> Vec<u8> {
                self.to_be_bytes().to_vec()
            }
            fn be16(self) -> [u8; 16] {
                let mut out = [0u8; 16];
                out[16 - $w..].copy_from_slice(&self.to_be_bytes());
                out
            }
            fn from_be(b: &[u8]) -> Option<Self> {
                if b.len() != $w {
                    return None;
                }
                let mut a = [0u8; $w];
                a.copy_from_slice(b);
                Some(<$t>::from_be_bytes(a))
            }
            fn as_u128(self) -> u128 {
                self as u128
            }
        }
    )+};
}

vocab_id!(u8 => 1, u16 => 2, u32 => 4, u64 => 8, u128 => 16);

/// An interned categorical label: string in memory, `N`-byte id on the wire.
pub struct Vocab<N: VocabId = u8> {
    /// The label. `None` = decoded from an id and not yet resolved (the deref
    /// step fills it); `Some("")` is a legal, distinct label — not a sentinel.
    name: Option<String>,
    /// The interned id. `None` = built from a label, not yet interned.
    id: Option<N>,
    _n: PhantomData<N>,
}

impl<N: VocabId> Vocab<N> {
    /// Bytes this field spends per row.
    pub const WIDTH: usize = N::WIDTH;

    /// Build from a label — the normal construction for a write. The value is
    /// not encodable until the write path interns it.
    pub fn of(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            id: None,
            _n: PhantomData,
        }
    }

    /// Reconstruct from the wire: the id is known, the label is not.
    pub fn from_id(id: N) -> Self {
        Self {
            name: None,
            id: Some(id),
            _n: PhantomData,
        }
    }

    /// The label, when this value has one (built from a label, or resolved
    /// after a decode).
    pub fn as_str(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The interned id, when this value has one.
    pub fn id(&self) -> Option<N> {
        self.id
    }

    /// Record the id the vocabulary handed out. Called by the intern hook.
    pub fn set_id(&mut self, id: N) {
        self.id = Some(id);
    }

    /// Fill the label after a decode. Called by the deref hook; a label already
    /// present is never overwritten (the constructor's label wins).
    pub fn set_name(&mut self, name: impl Into<String>) {
        if self.name.is_none() {
            self.name = Some(name.into());
        }
    }

    /// The id's big-endian image, right-aligned in 16 bytes: the last
    /// [`VocabId::WIDTH`] bytes are the wire form. Panics when the value was
    /// never interned — a missing id is a write-path bug (the derive's intern
    /// step is what prevents it), never something to paper over with a default.
    pub fn wire16(&self) -> [u8; 16] {
        match self.id {
            Some(id) => id.be16(),
            None => panic!(
                "Vocab::write_wire: `{}` was never interned — a Vocab field must go through \
                 Collection::put / save_with_prev_into (which intern before encoding), not \
                 through an encoding-only path with a hand-built document",
                self.name.as_deref().unwrap_or("<unnamed>")
            ),
        }
    }

    /// The wire form on its own ([`wire16`](Self::wire16), trimmed).
    pub fn to_wire(&self) -> Vec<u8> {
        let img = self.wire16();
        img[16 - N::WIDTH..].to_vec()
    }

    /// Inverse of [`to_wire`](Self::to_wire); `b` must be exactly
    /// [`VocabId::WIDTH`] bytes.
    pub fn from_wire(b: &[u8]) -> Self {
        assert!(
            b.len() == N::WIDTH,
            "Vocab wire is exactly {} byte(s), got {}",
            N::WIDTH,
            b.len()
        );
        Self::from_id(N::from_be(b).expect("Vocab: id out of range for its width"))
    }
}

impl<N: VocabId> Clone for Vocab<N> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            id: self.id,
            _n: PhantomData,
        }
    }
}

impl<N: VocabId> Default for Vocab<N> {
    /// The empty label, uninterned. A default `Vocab` encodes only after the
    /// intern step resolves it, exactly like any other built value.
    fn default() -> Self {
        Self::of(String::new())
    }
}

/// Equality is by LABEL when both sides carry one — that is what business code
/// means by "the same category". Two decoded values without labels compare by
/// id, and a named value never equals a nameless one (they are not the same
/// fact: one is resolved, the other is not).
impl<N: VocabId> PartialEq for Vocab<N> {
    fn eq(&self, other: &Self) -> bool {
        match (&self.name, &other.name) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.id == other.id,
            _ => false,
        }
    }
}

impl<N: VocabId> Eq for Vocab<N> {}

impl<N: VocabId> fmt::Debug for Vocab<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.name, self.id) {
            (Some(n), Some(i)) => write!(f, "Vocab({n:?} = {i:?})"),
            (Some(n), None) => write!(f, "Vocab({n:?}, uninterned)"),
            (None, Some(i)) => write!(f, "Vocab(id {i:?}, unresolved)"),
            (None, None) => write!(f, "Vocab(unresolved)"),
        }
    }
}

impl<N: VocabId> From<&str> for Vocab<N> {
    fn from(s: &str) -> Self {
        Self::of(s)
    }
}

impl<N: VocabId> From<String> for Vocab<N> {
    fn from(s: String) -> Self {
        Self::of(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_the_value_and_id_is_the_encoding() {
        let mut v: Vocab<u8> = Vocab::of("active");
        assert_eq!(v.as_str(), Some("active"));
        assert_eq!(v.id(), None);
        v.set_id(7);
        assert_eq!(v.to_wire(), vec![7]);
        assert_eq!(Vocab::<u8>::WIDTH, 1);
        // Round trip through the wire loses the label until deref fills it.
        let back = Vocab::<u8>::from_wire(&v.to_wire());
        assert_eq!(back.as_str(), None);
        assert_eq!(back.id(), Some(7));
    }

    #[test]
    fn a_wider_id_keeps_value_order_in_big_endian() {
        let mut v: Vocab<u16> = Vocab::of("archived");
        v.set_id(0x1234);
        assert_eq!(v.to_wire(), vec![0x12, 0x34]);
        assert_eq!(Vocab::<u16>::from_wire(&[0x12, 0x34]).id(), Some(0x1234));
        assert_eq!(Vocab::<u32>::WIDTH, 4);
        assert_eq!(Vocab::<u128>::WIDTH, 16);
    }

    #[test]
    fn equality_follows_the_label() {
        let mut a: Vocab<u8> = Vocab::of("active");
        let mut b: Vocab<u8> = Vocab::of("active");
        a.set_id(0);
        b.set_id(3);
        assert_eq!(a, b, "same label, different ids — same category");
        assert_ne!(a, Vocab::<u8>::of("closed"));
        // Resolved vs unresolved never compares equal: they are different facts.
        assert_ne!(a, Vocab::<u8>::from_id(0));
    }

    #[test]
    fn deref_fills_the_label_without_overwriting_a_built_one() {
        let mut decoded: Vocab<u8> = Vocab::from_id(2);
        decoded.set_name("suspended");
        assert_eq!(decoded.as_str(), Some("suspended"));
        decoded.set_name("ignored");
        assert_eq!(decoded.as_str(), Some("suspended"));
    }

    #[test]
    #[should_panic(expected = "was never interned")]
    fn encoding_an_uninterned_value_is_loud() {
        let v: Vocab<u8> = Vocab::of("active");
        let _ = v.to_wire();
    }

    #[test]
    #[should_panic(expected = "exactly 2 byte(s)")]
    fn from_wire_rejects_the_wrong_width() {
        let _ = Vocab::<u16>::from_wire(&[1]);
    }

    #[test]
    fn empty_label_is_a_label_not_a_sentinel() {
        let v: Vocab<u8> = Vocab::of("");
        assert_eq!(v.as_str(), Some(""));
        assert_ne!(v, Vocab::<u8>::of("x"));
    }
}
