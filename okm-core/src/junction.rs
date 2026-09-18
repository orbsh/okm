//! Junction side: the [`KvJunction`] trait and slot-addressed junction keys
//! (ADR-0015/0016 — supersedes the ADR-0001 direction-bit niche and the
//! ADR-0011 14/15 slot pair).
//!
//! SQL junction tables become explicit one-way entries, one per endpoint
//! ns: writing links both endpoints atomically (two entries), deleting
//! removes both. The junction struct's declaration doubles as the E-R
//! documentation — the relationship encoding and the key generation live
//! in the same place.
//!
//! Junction fields are [`okm_core::Ref`]`<D, K>` pairs: the document
//! parameter supplies the endpoint ns (`<D as Document>::NS_PREFIX`) and
//! its key type supplies the identity encoding. ns is declared once, on
//! the document; the junction re-states nothing. A document type may
//! anchor any number of junctions — including both endpoints of one
//! (self-reflexive follows/mentions graphs).
//!
//! Layout (each entry one-way, direction = which ns hosts it; both
//! identities in the entry — the LOCAL endpoint's identity leads so the
//! scan prefix can match, the PEER's identity is the suffix):
//! `[ns_a 2B BE][slot 2B BE: 0x3nnn][A·identity][B·identity]` in A's
//! collection, and `[ns_b 2B BE][slot 2B BE: 0x3nnn][B·identity][A·identity]`
//! in B's collection. The discriminator `nnn` comes from
//! `#[ok_junction(n)]` and separates multiple junctions over one endpoint
//! pair.

use crate::index::{JUNCTION_SLOT_BASE, Slot};
use crate::key::KeyEncode;

pub(crate) fn encode_head<K: KeyEncode>(buf: &mut Vec<u8>, key: &K, head: &[&str]) -> usize {
    if head.is_empty() {
        buf.extend_from_slice(&key.encode());
        K::KEY_LEN
    } else {
        key.encode_prefix_named(buf, head)
    }
}

pub(crate) fn head_width<K: KeyEncode>(head: &[&str]) -> usize {
    if head.is_empty() {
        K::KEY_LEN
    } else {
        K::prefix_width(head)
    }
}

/// Junction contract. `A` = first field (source document), `B` = second
/// field (destination document); both are document types — their
/// `NS_PREFIX` is the entry's ns, their `Key` the identity encoding.
/// `A_HEAD` / `B_HEAD` come from the `JunctionEncode` macro's
/// `#[ok_head(...)]` (empty = full identity).
pub trait KvJunction: Sized {
    type A: crate::index::Document;
    type B: crate::index::Document;
    /// Junction discriminator filling the segment-0x3 counter
    /// (`#[ok_junction(n)]`): separates multiple junctions over one
    /// endpoint pair.
    const JUNCTION_ID: u16;
    /// Identity fields of A for this junction.
    const A_HEAD: &'static [&'static str];
    /// Identity fields of B for this junction.
    const B_HEAD: &'static [&'static str];
    fn a(&self) -> &<Self::A as crate::index::Document>::Key;
    fn b(&self) -> &<Self::B as crate::index::Document>::Key;
    fn from_parts(
        a: <Self::A as crate::index::Document>::Key,
        b: <Self::B as crate::index::Document>::Key,
    ) -> Self;

    /// Slot for one direction: `0x3` segment, counter = discriminator
    /// shifted left one bit with the direction in the LSB. The bit is
    /// structurally necessary for self-reflexive junctions (both endpoints
    /// in one ns): without it the two directions' scan prefixes are
    /// identical and every scan cross-matches. Endpoint-distinct junctions
    /// simply never observe the bit — each ns only ever hosts one
    /// direction.
    fn dir_slot(dir: u16) -> Slot {
        JUNCTION_SLOT_BASE + (((Self::JUNCTION_ID & 0x07FF) << 1) | dir)
    }

    /// `[ns_a 2B BE][slot 2B BE][A·identity][B·identity]` — the fact seen
    /// from A's collection. The local identity (A) leads so the scan
    /// prefix can match; the peer identity (B) is the suffix.
    fn a_side_key(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(
            4 + head_width::<<Self::A as crate::index::Document>::Key>(Self::A_HEAD)
                + head_width::<<Self::B as crate::index::Document>::Key>(Self::B_HEAD),
        );
        k.extend_from_slice(<Self::A as crate::index::Document>::NS_PREFIX);
        k.extend_from_slice(&Self::dir_slot(0).to_be_bytes());
        encode_head(&mut k, self.a(), Self::A_HEAD);
        encode_head(&mut k, self.b(), Self::B_HEAD);
        k
    }

    /// `[ns_b 2B BE][slot 2B BE][B·identity][A·identity]` — the fact seen
    /// from B's collection. The local identity (B) leads.
    fn b_side_key(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(
            4 + head_width::<<Self::B as crate::index::Document>::Key>(Self::B_HEAD)
                + head_width::<<Self::A as crate::index::Document>::Key>(Self::A_HEAD),
        );
        k.extend_from_slice(<Self::B as crate::index::Document>::NS_PREFIX);
        k.extend_from_slice(&Self::dir_slot(1).to_be_bytes());
        encode_head(&mut k, self.b(), Self::B_HEAD);
        encode_head(&mut k, self.a(), Self::A_HEAD);
        k
    }

    /// Scan prefix in A's collection: `ns_a ++ slot ++ A·identity` (trailing
    /// partial bytes of A's identity are dropped — everything after A's
    /// identity belongs to an arbitrary B and must not participate in
    /// matching).
    fn a_side_prefix(a: &<Self::A as crate::index::Document>::Key) -> Vec<u8> {
        let mut p = Vec::with_capacity(
            4 + head_width::<<Self::A as crate::index::Document>::Key>(Self::A_HEAD),
        );
        p.extend_from_slice(<Self::A as crate::index::Document>::NS_PREFIX);
        p.extend_from_slice(&Self::dir_slot(0).to_be_bytes());
        encode_head(&mut p, a, Self::A_HEAD);
        p
    }

    /// Scan prefix in B's collection: `ns_b ++ slot ++ B·identity`.
    fn b_side_prefix(b: &<Self::B as crate::index::Document>::Key) -> Vec<u8> {
        let mut p = Vec::with_capacity(
            4 + head_width::<<Self::B as crate::index::Document>::Key>(Self::B_HEAD),
        );
        p.extend_from_slice(<Self::B as crate::index::Document>::NS_PREFIX);
        p.extend_from_slice(&Self::dir_slot(1).to_be_bytes());
        encode_head(&mut p, b, Self::B_HEAD);
        p
    }
}
