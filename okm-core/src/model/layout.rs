//! The layout: every number and every width a key is built from, in one file.
//!
//! A key in this engine is `[ns 2B][slot][payload…]`, and the payload's segments
//! are whatever the face's contract says they are — a handle, an interned id, a
//! clock, a hyper id. Those widths are the thing a reader gets wrong once and
//! never notices: a one-byte-vs-two-byte disagreement does not fail, it reads the
//! wrong row. So they live here, named, and every key builder in `model::graph`,
//! `model::hyper`, `model::handles` and `model::obj_vocab` asks this module
//! instead of spelling a number.
//!
//! **Changing a width is a change to this file.** Everything else that has to
//! follow — `HandleKey::WIDTH`, the member-row suffix arithmetic, the vocabulary
//! id type — is expressed in terms of these constants, so it follows by
//! construction rather than by grep.
//!
//! # The fixed faces (one byte, 0x00–0x1F)
//!
//! ```text
//! 0x00  primary                    document collection or edge collection
//! 0x01  dynamic segment            [ns][0x01][pkey]
//! 0x02  field-name vocabulary      id → name
//! 0x03  field-name vocabulary      name → id
//! 0x04  predicate vocabulary       id → name
//! 0x05  predicate vocabulary       name → id
//! 0x06  graph out (the edge)       [src_h][pred][dst_h]  → the edge payload
//! 0x07  graph in                   [dst_h][pred][src_h]  → empty, derived
//! 0x08  graph predicate+out        [pred][src_h][dst_h]  → empty, derived
//! 0x09  graph predicate+in         [pred][dst_h][src_h]  → empty, derived
//! 0x0A  cluster-kind vocabulary    id → name
//! 0x0B  cluster-kind vocabulary    name → id
//! 0x0C  cluster                    [kind][hyper_id]      → [parent 8B][payload]
//! 0x0D  edge member                [kind][hyper_id][src_h][pred][dst_h] → empty
//! 0x0E  child cluster index        [kind][parent][child] → empty, derived
//! 0x0F  edge → cluster index       [src_h][pred][dst_h][kind][hyper_id] → empty
//! 0x10  handle → node              [handle] → [node_ns 2B][pkey]
//! 0x11  node → handle              [node_ns 2B][pkey] → [handle]
//! 0x12–0x1D  unassigned            the band's only reserve, one contiguous block
//! 0x1E  Extension                  [subkind]… each subkind owns its contract
//! 0x1F  table meta                 → name → bytes map
//! ```
//!
//! The order is not decoration: a vocabulary precedes the faces that read its
//! ids (0x02/0x03 → payload names, 0x04/0x05 → predicates, 0x0A/0x0B → cluster
//! kinds), so a reader never has to learn a table's private convention to decode
//! a row. ADR-0030 is this table's prose.

use crate::model::index::Slot;

// ---------------------------------------------------------------------------
// Widths
// ---------------------------------------------------------------------------

/// The namespace id's width: every key starts `[ns 2B]`.
pub const NS_WIDTH: usize = core::mem::size_of::<u16>();

/// The fixed-width 64-bit segments — a handle, a hyper id, a cluster parent, a
/// clock.
pub const U64_WIDTH: usize = core::mem::size_of::<u64>();

/// A node's handle inside a graph: dense, issued on first endpoint sight.
pub const HANDLE_WIDTH: usize = core::mem::size_of::<u64>();

/// Field-name vocabulary ids (0x02/0x03): the names inside a payload.
pub type FieldNameId = u16;
/// How many bytes a [`FieldNameId`] takes in a key.
pub const FIELD_VOCAB_WIDTH: usize = core::mem::size_of::<FieldNameId>();

/// Predicate vocabulary ids (0x04/0x05): a graph's predicates, and the segment
/// an edge tuple carries between its two handles.
pub type PredicateId = u16;
/// How many bytes a [`PredicateId`] takes in a key — the edge tuple's middle.
pub const PREDICATE_VOCAB_WIDTH: usize = core::mem::size_of::<PredicateId>();

/// Cluster-kind vocabulary ids (0x0A/0x0B): the labels a caller interns for its
/// own groupings.
pub type ClusterKindId = u16;
/// How many bytes a [`ClusterKindId`] takes in a key.
pub const CLUSTER_KIND_WIDTH: usize = core::mem::size_of::<ClusterKindId>();

// ---------------------------------------------------------------------------
// The fixed faces
// ---------------------------------------------------------------------------

/// The collection's primary row.
pub const PRIMARY_SLOT: Slot = 0x00;
/// The document's dynamic segment (its open attributes).
pub const DYNAMIC_SLOT: Slot = 0x01;

/// Field-name vocabulary, id → name. Carries the allocation cursor at the bare
/// prefix `[ns][0x02]`.
pub const FIELD_VOCAB_ID_SLOT: Slot = 0x02;
/// Field-name vocabulary, name → id.
pub const FIELD_VOCAB_NAME_SLOT: Slot = 0x03;

/// Predicate vocabulary, id → name. Carries its cursor at `[ns][0x04]`.
pub const PREDICATE_VOCAB_ID_SLOT: Slot = 0x04;
/// Predicate vocabulary, name → id.
pub const PREDICATE_VOCAB_NAME_SLOT: Slot = 0x05;

/// The graph's forward edge face — the edge row itself (ADR-0067).
pub const EDGE_OUT_SLOT: Slot = 0x06;
/// The reverse edge face, derived from `0x06`.
pub const EDGE_IN_SLOT: Slot = 0x07;
/// The edge set ordered by predicate first, derived.
pub const EDGE_PREDICATE_OUT_SLOT: Slot = 0x08;
/// The reverse of `0x08`, derived.
pub const EDGE_PREDICATE_IN_SLOT: Slot = 0x09;

/// Cluster-kind vocabulary, id → name. Carries its cursor at `[ns][0x0A]`.
pub const CLUSTER_KIND_ID_SLOT: Slot = 0x0A;
/// Cluster-kind vocabulary, name → id.
pub const CLUSTER_KIND_NAME_SLOT: Slot = 0x0B;

/// The cluster row: `[kind][hyper_id]` → `[parent u64 BE][dynamic payload]`.
pub const CLUSTER_SLOT: Slot = 0x0C;
/// One row per member EDGE — the authority for membership.
pub const EDGE_MEMBER_SLOT: Slot = 0x0D;
/// The derived child index `[kind][parent][child]`.
pub const CHILD_INDEX_SLOT: Slot = 0x0E;
/// The derived EDGE → CLUSTER index `[src_h][pred][dst_h][kind][hyper_id]`.
pub const EDGE_CLUSTER_SLOT: Slot = 0x0F;

/// The handle registry's forward face: `[handle]` → `[node_ns][pkey]`.
pub const HANDLE_NODE_SLOT: Slot = 0x10;
/// The handle registry's reverse face, derived from `0x10`.
pub const NODE_HANDLE_SLOT: Slot = 0x11;

/// The Extension face: each subkind owns its own contract (ADR-0030).
pub const EXTENSION_SLOT: Slot = 0x1E;
/// The table's META entry — name → bytes (ADR-0043).
pub const META_SLOT: Slot = 0x1F;

/// Bits of a two-byte slot that hold the counter; the rest is the family.
pub const SLOT_COUNTER_BITS: u32 = 12;
/// The largest counter a family slot can carry (`0x0FFF`), which is why a
/// declaration-order index is capped at 4095 per collection.
pub const SLOT_COUNTER_MAX: u16 = (1 << SLOT_COUNTER_BITS) - 1;
/// The lowest family number: `0x0` is the fixed band and `0x1` would encode
/// inside it, so the counting families start at `0x2`.
pub const SLOT_FAMILY_MIN: u16 = 0x2;
/// The highest family number a slot may carry.
pub const SLOT_FAMILY_MAX: u16 = 0x000F;

/// The highest one-byte face. `slot <= FIXED_FACE_MAX` is one byte; anything
/// above is a two-byte counting-family slot.
pub const FIXED_FACE_MAX: Slot = 0x1F;

#[cfg(test)]
mod tests {
    use super::*;

    /// The band is one contiguous run with no gaps inside it and no duplicate
    /// numbers: what makes "every slot is classified" checkable.
    #[test]
    fn the_faces_are_distinct_and_inside_the_band() {
        let faces = [
            PRIMARY_SLOT,
            DYNAMIC_SLOT,
            FIELD_VOCAB_ID_SLOT,
            FIELD_VOCAB_NAME_SLOT,
            PREDICATE_VOCAB_ID_SLOT,
            PREDICATE_VOCAB_NAME_SLOT,
            EDGE_OUT_SLOT,
            EDGE_IN_SLOT,
            EDGE_PREDICATE_OUT_SLOT,
            EDGE_PREDICATE_IN_SLOT,
            CLUSTER_KIND_ID_SLOT,
            CLUSTER_KIND_NAME_SLOT,
            CLUSTER_SLOT,
            EDGE_MEMBER_SLOT,
            CHILD_INDEX_SLOT,
            EDGE_CLUSTER_SLOT,
            HANDLE_NODE_SLOT,
            NODE_HANDLE_SLOT,
            EXTENSION_SLOT,
            META_SLOT,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for face in faces {
            assert!(face <= FIXED_FACE_MAX, "0x{face:02X} is outside the band");
            assert!(seen.insert(face), "0x{face:02X} is used twice");
        }
        assert_eq!(
            faces.iter().copied().max(),
            Some(META_SLOT),
            "the meta sits on the band's boundary, so nothing can squeeze it out"
        );
    }

    /// The widths are what the arithmetic says they are, so the key builders can
    /// add them instead of spelling 2, 8 and 18.
    #[test]
    fn the_widths_compose() {
        assert_eq!(NS_WIDTH, 2);
        assert_eq!(U64_WIDTH, 8);
        assert_eq!(HANDLE_WIDTH, 8);
        assert_eq!(FIELD_VOCAB_WIDTH, 2);
        assert_eq!(PREDICATE_VOCAB_WIDTH, 2);
        assert_eq!(CLUSTER_KIND_WIDTH, 2);
        // An edge tuple: two handles and a predicate.
        assert_eq!(2 * HANDLE_WIDTH + PREDICATE_VOCAB_WIDTH, 18);
    }
}