//! The graph's own identity: a dense `u64` handle per node, issued when the
//! node first appears as an endpoint and resolved to its `(ns, pkey)` only at
//! the boundary (ADR-0067, "a boundary-only handle").
//!
//! ```text
//! 0x0E  handle → node   [ns][0x0E][handle u64]         → [node_ns 2B][pkey]
//! 0x0F  node → handle   [ns][0x0F][node_ns 2B][pkey]   → [handle u64 BE]
//! ```
//!
//! Two FIXED FACES, one byte each, adjacent in the core band: the direction IS
//! the face number, so no key carries a discriminator byte. They sit
//! immediately after the core (`0x00`–`0x0D`) because the band is packed from
//! `0x00`: the pair took the two numbers the tail used to hold (Extension and
//! the meta moved up to `0x1E`/`0x1F`), and the band's one reserve —
//! `0x10`–`0x1D` — sits ABOVE the pair, so the next face added takes `0x10`
//! and nothing has to be renumbered (ADR-0030).
//!
//! Before the band widened they were ONE counting family (`0xF00n` with the
//! direction in the counter bits), because sixteen fixed faces were spent and
//! `0x10`/`0x11` were not encodable at all. Thirty-two faces make the pair
//! affordable: no family number, no discriminator, and the adjacency is real
//! rather than numeric coincidence — the forward face's scan range ends exactly
//! where the reverse face's begins, which is what makes "one row per direction,
//! one registry" visible in the key space.
//!
//! The registry sits in the CORE band because it is the graph's own structure,
//! not an extension of a document: two rows per node — one per direction — and
//! the traversal faces then carry `[src_h][pred][dst_h]`, eighteen bytes
//! whatever width a member table's key is. A composite `(u32, u64, u32)`
//! identity costs thirty-eight bytes as a tuple, and the tuple appears four
//! times per edge, so the registry pays for itself at a degree below one.
//!
//! # Lifecycle
//!
//! A handle is issued monotonically from [`FIRST_HANDLE`] the first time a node
//! appears as an endpoint of a fragment's edge, and `0` is reserved as
//! [`Handle::NONE`] — "no handle", the way the cluster id reserves zero for "no
//! parent". Deletes do NOT reclaim: a handle outlives its last edge only as
//! waste, because the alternative is reference counting, a resource the model
//! does not otherwise have. Waste is bounded by the number of nodes that have
//! ever had an edge, and `2^64` makes it a space question, not a correctness
//! one. [`crate::model::hyper::Hyperedges::rebuild_derived`] rewrites the
//! reverse face from the forward one; nothing reissues handles (a compaction,
//! when it exists, would reissue from zero, ADR-0067).
//!
//! # The cursor is a face-level singleton
//!
//! `[ns][0x0E]` with no suffix holds the next handle — the bare face prefix,
//! the same shape the vocabulary cursors use (`obj_vocab::cursor_key`), and
//! legal for the same reason: every real entry of that face is exactly eight
//! bytes after the prefix, so a zero-length suffix is not expressible as data.
//! A reader tells the two apart by LENGTH, and an eight-byte decoder rejects
//! the cursor instead of reading it as a handle.
//!
//! Ordering is issue order, not key order: a range over handles is a range over
//! "when this node first appeared", which no face may rely on for value
//! ordering. A handle is therefore never a key-order proxy.

use crate::engine::storage::{Batch, BatchStore, VirtualStorage, prefix_end, scan_suffix_kv};
use crate::error::{Error, Result};
use crate::model::graph::NodeRef;
use crate::model::index::{Slot, write_slot};

/// `[ns][0x0E][handle u64] → [node_ns 2B][pkey]` — the forward face, which is
/// authority. Also carries the allocation cursor as its bare-prefix singleton.
pub const HANDLE_NODE_SLOT: Slot = 0x0E;
/// `[ns][0x0F][node_ns 2B][pkey] → [handle u64 BE]` — the reverse direction,
/// derived from the forward face and rebuildable from it alone.
pub const NODE_HANDLE_SLOT: Slot = 0x0F;

/// The first handle ever issued. Zero is [`Handle::NONE`].
pub const FIRST_HANDLE: u64 = 1;

/// A node's identity INSIDE the graph: a dense `u64`, issued on first endpoint
/// sight.
///
/// It is deliberately not [`NodeRef`]: a handle names a row of the graph's own
/// registry, carries no type tag and no key, and means nothing outside this
/// graph's namespace. [`Handle::NONE`] is the absent value — the reservation
/// that keeps `0` out of the numbering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle(u64);

impl Handle {
    /// The absent handle: never issued, and parsing it answers `None`.
    pub const NONE: Handle = Handle(0);

    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw number.
    pub const fn id(self) -> u64 {
        self.0
    }

    /// Is this the reserved absent value?
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Append the eight big-endian bytes.
    pub fn write(self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.0.to_be_bytes());
    }

    /// The eight big-endian bytes.
    pub fn wire(self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }

    /// Read a handle from EXACTLY eight bytes. A zero (the never-issued value)
    /// or any other width is not a handle, so this answers `None` rather than
    /// inventing one.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let raw = <[u8; 8]>::try_from(bytes).ok()?;
        let id = u64::from_be_bytes(raw);
        (id >= FIRST_HANDLE).then_some(Self(id))
    }
}

/// `[ns][0x10][handle]` — one handle's node row.
pub fn handle_key(ns: &[u8; 2], handle: Handle) -> Vec<u8> {
    let mut key = handle_prefix(ns);
    handle.write(&mut key);
    key
}

/// `[ns][0x11][node_ns][pkey]` — one node's handle row.
pub fn node_handle_key(ns: &[u8; 2], node: &NodeRef) -> Vec<u8> {
    let mut key = node_handle_prefix(ns);
    node.write(&mut key);
    key
}

/// `[ns][0x10]` — the handle face's prefix, and the cursor's key.
pub fn handle_prefix(ns: &[u8; 2]) -> Vec<u8> {
    let mut key = ns.to_vec();
    write_slot(&mut key, HANDLE_NODE_SLOT);
    key
}

/// `[ns][0x11]` — the reverse face's prefix.
pub fn node_handle_prefix(ns: &[u8; 2]) -> Vec<u8> {
    let mut key = ns.to_vec();
    write_slot(&mut key, NODE_HANDLE_SLOT);
    key
}

/// The allocation cursor's key: the handle face's bare prefix, holding the next
/// handle. One definition for every reader and writer — the allocator and the
/// rebuild.
pub fn cursor_key(ns: &[u8; 2]) -> Vec<u8> {
    handle_prefix(ns)
}

/// The node row's value: `[node_ns 2B][pkey]`, no length (`KeyEncode` fixed the
/// width at declaration time, and a registry row is read by its reader, not
/// parsed in the middle of another key).
pub fn node_value(node: &NodeRef) -> Vec<u8> {
    node.wire()
}

fn node_of(value: &[u8]) -> Option<NodeRef> {
    if value.len() < 2 {
        return None;
    }
    Some(NodeRef::new(&value[..2], &value[2..]))
}

fn be_u64(bytes: &[u8]) -> Option<u64> {
    <[u8; 8]>::try_from(bytes).ok().map(u64::from_be_bytes)
}

/// Engine + namespace = the handle registry of one graph.
///
/// Reads are point reads ([`handle_of`](Self::handle_of),
/// [`node`](Self::node)) or ONE range scan ([`nodes`](Self::nodes)); the write
/// is [`issue_into`](Self::issue_into), which rides a caller-owned batch so a
/// refused fragment leaves no handle behind.
pub struct Handles<'a, S> {
    store: &'a S,
    ns: [u8; 2],
}

impl<'a, S: VirtualStorage> Handles<'a, S> {
    /// The registry of the graph rooted at `ns`.
    pub fn new(store: &'a S, ns: [u8; 2]) -> Self {
        Self { store, ns }
    }

    /// The namespace the registry lives in.
    pub fn ns(&self) -> u16 {
        u16::from_be_bytes(self.ns)
    }

    /// The engine handle.
    pub fn store(&self) -> &'a S {
        self.store
    }

    /// One node's handle, if the node ever appeared as an endpoint — one point
    /// read. `None` is a node the graph has never seen, which is a legitimate
    /// answer (it has no edges), never an error.
    pub fn handle_of(&self, node: &NodeRef) -> Result<Option<Handle>> {
        match self.store.get(&node_handle_key(&self.ns, node))? {
            Some(raw) => match Handle::parse(&raw) {
                Some(handle) => Ok(Some(handle)),
                None => Err(Error::Corrupt {
                    what: format!(
                        "the node → handle row for ns {} is {} byte(s); a handle is 8",
                        node.ns_id(),
                        raw.len()
                    ),
                }),
            },
            None => Ok(None),
        }
    }

    /// One handle's node — one point read. `None` when the handle was never
    /// issued (or the registry row is missing), never a guessed ref.
    pub fn node(&self, handle: Handle) -> Result<Option<NodeRef>> {
        if handle.is_none() {
            return Ok(None);
        }
        Ok(self
            .store
            .get(&handle_key(&self.ns, handle))?
            .and_then(|raw| node_of(&raw)))
    }

    /// Every entry of the registry, issue order: one scan of 0x10. The cursor
    /// row (a zero-length suffix) is not an entry and is skipped by width.
    pub fn all(&self) -> Result<Vec<(Handle, NodeRef)>> {
        let mut out = Vec::new();
        for (suffix, value) in scan_suffix_kv(self.store, &handle_prefix(&self.ns))? {
            let (Some(handle), Some(node)) = (Handle::parse(&suffix), node_of(&value)) else {
                continue;
            };
            out.push((handle, node));
        }
        Ok(out)
    }

    /// The next handle the cursor names — the allocation watermark, without
    /// allocating. Absent (or zero) means nothing has been issued yet.
    pub fn next_handle(&self) -> Result<Handle> {
        Ok(match self.store.get(&cursor_key(&self.ns))? {
            Some(raw) => match be_u64(&raw) {
                Some(next) if next >= FIRST_HANDLE => Handle(next),
                _ => Handle(FIRST_HANDLE),
            },
            None => self.max_plus_one()?,
        })
    }

    /// The one-time fallback for a registry whose cursor is missing: the highest
    /// issued handle plus one. Never reached once the cursor is written, and
    /// never a reissue of a live handle.
    fn max_plus_one(&self) -> Result<Handle> {
        let mut next = Handle(FIRST_HANDLE);
        for (suffix, _) in scan_suffix_kv(self.store, &handle_prefix(&self.ns))? {
            if let Some(handle) = Handle::parse(&suffix) {
                next = next.max(Handle(handle.id().saturating_add(1)));
            }
        }
        Ok(next)
    }

    /// The node's handle, ISSUING it on first sight — the write path's one
    /// entry.
    ///
    /// The probe reads through `batch`, so a node named twice in one fragment
    /// answers the handle the same fragment already wrote (no second row), and
    /// the three writes land in that batch: a refused fragment commits none of
    /// them, so it leaves no handle behind. Allocated handles are never
    /// reclaimed (the type's lifecycle note).
    pub fn issue_into(&self, batch: &mut Batch, node: &NodeRef) -> Result<Handle> {
        let next = {
            let pending = BatchStore::new(self.store, batch);
            match pending.get(&node_handle_key(&self.ns, node))? {
                Some(raw) => {
                    return Handle::parse(&raw).ok_or_else(|| Error::Corrupt {
                        what: format!(
                            "the node → handle row for ns {} is {} byte(s); a handle is 8",
                            node.ns_id(),
                            raw.len()
                        ),
                    });
                }
                None => match pending.get(&cursor_key(&self.ns))? {
                    Some(raw) => match be_u64(&raw) {
                        Some(next) if next >= FIRST_HANDLE => Handle(next),
                        _ => Handle(FIRST_HANDLE),
                    },
                    None => Handle(FIRST_HANDLE),
                },
            }
        };
        let Some(after) = next.id().checked_add(1) else {
            return Err(Error::VocabularyExhausted {
                width: 8,
                next: next.id() as u128 + 1,
            });
        };
        batch.put(handle_key(&self.ns, next), node_value(node));
        batch.put(node_handle_key(&self.ns, node), next.wire());
        batch.put(cursor_key(&self.ns), after.to_be_bytes().to_vec());
        Ok(next)
    }

    /// Resolve a SORTED set of handles in ONE range scan — the bulk answer for
    /// a result set, which is sorted by construction because handles are issued
    /// monotonically (`EdgeKey` order is a different order; sort first).
    ///
    /// The scan spans first → last, so a sparse set pays for its span. Answers
    /// one ref per input entry, in input order; a handle no row holds is
    /// [`Error::Corrupt`], because a caller that asked for it read it off a face
    /// that promises it exists.
    pub fn nodes(&self, handles: &[Handle]) -> Result<Vec<NodeRef>> {
        if handles.is_empty() {
            return Ok(Vec::new());
        }
        debug_assert!(
            handles.windows(2).all(|pair| pair[0] <= pair[1]),
            "the bulk resolver takes a sorted handle set — sort it at the call site"
        );
        let first = handles[0];
        let last = *handles.last().unwrap_or(&first);
        // The range START is the first handle's whole key — a lower bound, not a
        // prefix — and the range END is the face's own bound, so the scan still
        // spans first → last and a sparse set pays for its span rather than for
        // the face. The STRIP, though, is the face prefix: `scan_suffix_kv` would
        // take `handle_key(first)` as its prefix and hand back an empty suffix
        // for every entry, which is why the slicing is done here, at the face's
        // width, against the keys `scan_range` returns unmodified.
        let prefix = handle_prefix(&self.ns);
        let begin = handle_key(&self.ns, first);
        let mut found = Vec::new();
        for item in self
            .store
            .scan_range(&begin, prefix_end(&prefix).as_deref())
        {
            let (key, value) = item?;
            let Some(suffix) = key.get(prefix.len()..) else {
                continue;
            };
            let (Some(handle), Some(node)) = (Handle::parse(suffix), node_of(&value)) else {
                continue;
            };
            if handle > last {
                continue;
            }
            found.push((handle, node));
        }
        let mut out = Vec::with_capacity(handles.len());
        for wanted in handles {
            match found.iter().find(|(handle, _)| handle == wanted) {
                Some((_, node)) => out.push(node.clone()),
                None => {
                    return Err(Error::Corrupt {
                        what: format!(
                            "handle {} is named by an edge row but the registry has no node for it",
                            wanted.id()
                        ),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Rewrite the REVERSE face (0x11) from the forward one (0x10) and restate
    /// the cursor — the derived half of a rebuild. Answers the rows written.
    ///
    /// 0x10 is authority: it holds `[node_ns][pkey]`, which nothing else in the
    /// graph carries any more (an edge row names handles, not refs), so the
    /// reverse face is what a rebuild derives. Handles nothing names are NOT
    /// reclaimed — that is compaction's business, not a rebuild's.
    pub fn rebuild_reverse(&self) -> Result<usize> {
        let entries = self.all()?;
        let mut batch = Batch::default();
        // Sweep the reverse face: an entry's key encodes the node it names, and
        // nothing but this face's own derivation writes it, so re-deriving is
        // the repair and the sweep is the coverage.
        let prefix = node_handle_prefix(&self.ns);
        for item in self.store.scan_range(
            &prefix,
            crate::engine::storage::prefix_end(&prefix).as_deref(),
        ) {
            let (key, _) = item?;
            batch.del(&key);
        }
        for (handle, node) in &entries {
            batch.put(node_handle_key(&self.ns, node), handle.wire());
        }
        if let Some((last, _)) = entries.last()
            && let Some(after) = last.id().checked_add(1)
        {
            batch.put(cursor_key(&self.ns), after.to_be_bytes().to_vec());
        }
        self.store.commit_batch(batch)?;
        Ok(entries.len())
    }
}
