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
//! Layout (ADR-0050 — a junction is a table, not two parasitized queues):
//! the junction OWNS one ns. Its primary face `0x00` holds
//! `[ns 2B BE][slot 0x00][A·identity][B·identity]` — the relation row, and the only place
//! its data lives. Its reverse face reuses fixed face `0x06` (free: an ns is a graph OR a
//! junction) as `[ns][0x06][B·identity][A·identity]`, an index rebuildable from the primary
//! face. `#[ok_ns(N)]` declares the ns; the 0x5nnn segment, its 11-bit discriminator and the
//! direction bit are gone.

use crate::model::index::{write_slot, Slot};

/// A junction's primary face: the relation row, at the same 0x00 a document collection uses.
const TABLE_PRIMARY_SLOT: Slot = 0x00;
use crate::model::key::KeyEncode;

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
    type A: crate::model::index::Document;
    type B: crate::model::index::Document;
    /// The junction's own namespace, declared with `#[ok_ns(N)]` (ADR-0050): one relation
    /// type = one ns = one table.
    const NS: u16;
    /// Identity fields of A for this junction.
    const A_HEAD: &'static [&'static str];
    /// Identity fields of B for this junction.
    const B_HEAD: &'static [&'static str];
    fn a(&self) -> &<Self::A as crate::model::index::Document>::Key;
    fn b(&self) -> &<Self::B as crate::model::index::Document>::Key;
    fn from_parts(
        a: <Self::A as crate::model::index::Document>::Key,
        b: <Self::B as crate::model::index::Document>::Key,
    ) -> Self;


    /// `[ns_a 2B BE][slot 2B BE][A·identity][B·identity]` — the fact seen
    /// from A's collection. The local identity (A) leads so the scan
    /// prefix can match; the peer identity (B) is the suffix.
    fn a_side_key(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(
            4 + head_width::<<Self::A as crate::model::index::Document>::Key>(Self::A_HEAD)
                + head_width::<<Self::B as crate::model::index::Document>::Key>(Self::B_HEAD),
        );
        k.extend_from_slice(&Self::NS.to_be_bytes());
        write_slot(&mut k, TABLE_PRIMARY_SLOT);
        encode_head(&mut k, self.a(), Self::A_HEAD);
        encode_head(&mut k, self.b(), Self::B_HEAD);
        k
    }

    /// `[ns_b 2B BE][slot 2B BE][B·identity][A·identity]` — the fact seen
    /// from B's collection. The local identity (B) leads.
    fn b_side_key(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(
            4 + head_width::<<Self::B as crate::model::index::Document>::Key>(Self::B_HEAD)
                + head_width::<<Self::A as crate::model::index::Document>::Key>(Self::A_HEAD),
        );
        k.extend_from_slice(&Self::NS.to_be_bytes());
        write_slot(&mut k, crate::model::graph::EDGE_OUT_SLOT);
        encode_head(&mut k, self.b(), Self::B_HEAD);
        encode_head(&mut k, self.a(), Self::A_HEAD);
        k
    }

    /// Scan prefix in A's collection: `ns_a ++ slot ++ A·identity` (trailing
    /// partial bytes of A's identity are dropped — everything after A's
    /// identity belongs to an arbitrary B and must not participate in
    /// matching).
    fn a_side_prefix(a: &<Self::A as crate::model::index::Document>::Key) -> Vec<u8> {
        let mut p = Vec::with_capacity(
            4 + head_width::<<Self::A as crate::model::index::Document>::Key>(Self::A_HEAD),
        );
        p.extend_from_slice(&Self::NS.to_be_bytes());
        write_slot(&mut p, TABLE_PRIMARY_SLOT);
        encode_head(&mut p, a, Self::A_HEAD);
        p
    }

    /// Scan prefix in B's collection: `ns_b ++ slot ++ B·identity`.
    fn b_side_prefix(b: &<Self::B as crate::model::index::Document>::Key) -> Vec<u8> {
        let mut p = Vec::with_capacity(
            4 + head_width::<<Self::B as crate::model::index::Document>::Key>(Self::B_HEAD),
        );
        p.extend_from_slice(&Self::NS.to_be_bytes());
        write_slot(&mut p, crate::model::graph::EDGE_OUT_SLOT);
        encode_head(&mut p, b, Self::B_HEAD);
        p
    }
}
