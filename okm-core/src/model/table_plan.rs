//! What a declaration compiles to on the write path (ADR-0074 §3).
//!
//! Attach answers three questions once — what a key of this table starts with, which slots the
//! log carries, and what the declaration numbered them — and hands the answer to the log as a
//! value. The log then indexes a table instead of re-deriving the answer from bytes, which is
//! what keeps the cost of a commit independent of how many tables a store holds.
//!
//! It is a CONST, not a cache. Everything a plan holds is known at compile time: the ns prefix
//! is `Document::NS_PREFIX`, the slot numbers are the declaration's, and the capture rule is
//! arithmetic over them. So there is nothing to build, nothing to lock, and no second copy to
//! drift out of step — `Document::PLAN` IS the plan, evaluated by the same compiler that
//! evaluated the declaration.
//!
//! The one case with no type to ask is the dynamic path (a schema read back from a store, a
//! CLI with no declaration compiled in). [`TablePlan::compile`] builds the same value at run
//! time from the declaration it read.

use crate::model::index::Slot;
use crate::model::schema::{CollectionSchema, SlotMap};

/// Slots `0x0000..=0x00FF` fit the bitmap — every fixed face, and every standard family.
const BITMAP_SLOTS: usize = 256;

#[derive(Clone, Copy, Debug)]
pub struct TablePlan {
    /// The key prefix every write of this table shares (the ns, two bytes).
    header: &'static [u8],
    /// One bit per slot, low slot in the low bit: set = the log carries that slot.
    sides: [u64; BITMAP_SLOTS / 64],
    /// The declaration's slot numbers, for what the bitmap cannot hold (extended faces).
    slots: SlotMap,
}

impl TablePlan {
    /// Compile a declaration at compile time — the constructor `Document::PLAN` uses.
    ///
    /// No sweep over 0..=255 is needed, because within that range the rule's answer is a
    /// NAMED slot every time: the two truth families (`slot >> 12`) live at 4096 and up, and
    /// [`Self::carries`] asks the rule above the bitmap. The two answers therefore agree by
    /// construction rather than by discipline.
    pub const fn of(header: &'static [u8], slots: SlotMap) -> Self {
        let mut sides = [0u64; BITMAP_SLOTS / 64];
        let named: [Slot; 10] = [
            slots.primary,
            slots.dynamic,
            slots.field_vocab_id,
            slots.field_vocab_name,
            slots.predicate_vocab_id,
            slots.predicate_vocab_name,
            crate::model::index::CLUSTER_KIND_ID_SLOT,
            crate::model::index::CLUSTER_KIND_NAME_SLOT,
            crate::model::layout::CLUSTER_SLOT,
            crate::model::layout::EDGE_MEMBER_SLOT,
        ];
        let mut i = 0;
        while i < named.len() {
            let slot = named[i] as usize;
            if slot < BITMAP_SLOTS {
                sides[slot / 64] |= 1 << (slot % 64);
            }
            i += 1;
        }
        Self {
            header,
            sides,
            slots,
        }
    }

    /// The same value for a declaration that arrived as bytes — the dynamic path, where there
    /// is no type to ask and no const to point at.
    ///
    /// The header is the ns (two bytes) and it is leaked: a plan is `'static` by construction,
    /// and a schema read back from a store is declared once and used for the life of the
    /// process. Two bytes per distinct declaration is the whole price of keeping the static
    /// form's shape instead of a lifetime parameter on every table.
    pub fn compile(schema: &CollectionSchema) -> Self {
        // The generation is one bit of the address (ADR-0075 §2), and a schema carries the
        // number it was compiled from — so a plan built from stored bytes names the same
        // address the declaration does, byte for byte.
        let mut ns = schema.ns_prefix.clone();
        if let Some(first) = ns.first_mut() {
            *first = (*first & 0x7F) | ((schema.layout_version & 1) << 7);
        }
        let header: &'static [u8] = Box::leak(ns.into_boxed_slice());
        Self::of(header, schema.slots)
    }

    /// The key prefix a write of this table carries.
    pub fn header(&self) -> &'static [u8] {
        self.header
    }

    /// Does the log carry this slot? One test and one shift for a fixed face; the rule again
    /// only for the extended faces the bitmap has no room for.
    pub fn carries(&self, slot: Slot) -> bool {
        let index = slot as usize;
        if index < BITMAP_SLOTS {
            return self.sides[index / 64] & (1 << (index % 64)) != 0;
        }
        crate::changelog::side_of(&self.slots, slot) == crate::changelog::Side::Truth
    }

    /// Does this log carry this key? The slot is read out of the bytes — the one field the key
    /// carries positionally — and the answer comes from the compiled table.
    pub fn carries_key(&self, key: &[u8]) -> bool {
        let Some(rest) = key.strip_prefix(self.header) else {
            // Another table's key in a cross-collection batch: not ours to write.
            return false;
        };
        let Some(&first) = rest.first() else {
            return false;
        };
        let (slot, width) = if first <= crate::model::layout::FIXED_FACE_MAX as u8 {
            (Slot::from(first), 1usize)
        } else {
            let Some(&second) = rest.get(1) else {
                return false;
            };
            (u16::from_be_bytes([first, second]), 2usize)
        };
        if rest.len() == width {
            // A face's bare prefix is a singleton (a vocabulary's allocation cursor, the
            // table's meta row): no row has that shape.
            return false;
        }
        self.carries(slot)
    }
}
