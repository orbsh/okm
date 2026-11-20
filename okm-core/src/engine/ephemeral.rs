//! The EPHEMERAL engine: okm's NON-PERSISTENT MODE.
//!
//! A model is declared once — keys, indexes, faces, scans, reduces — and the
//! engine is chosen at assembly. Durability is part of that choice, not part of
//! the model: this store is the same declarations over an engine that keeps
//! everything and promises nothing. That is what lets a system with a real
//! model run a computation without persistence, instead of growing a second
//! implementation of the model beside the durable one (ADR-0072).
//!
//! Its first consumer is aura, a compute engine that stores its state in okm
//! and runs computations that do not persist; a temporary realm (`create_ephemeral`,
//! aura ADR-0050 §6) is the common case of that, not the definition. `okm-core`'s
//! unit tests are another caller, for a cargo reason (a dev-dependency leading
//! back to this package makes cargo build it twice, and the two copies'
//! `VirtualStorage` traits are distinct types — ADR-0072 §1).
//!
//! What it therefore does NOT do, and why each is absent rather than deferred:
//!
//! - **No journal, no recovery.** Nothing it writes outlives the handle. A
//!   crash loses the store, which is what a computation's scratch state
//!   already promises.
//! - **No compaction, generations, or migration.** Those are rules about bytes
//!   at rest (ADR-0042, ADR-0056); there are none. The one housekeeping job it
//!   owns is the rebuild ([`Inner::rebuild`]) — space freed by deletes is
//!   reclaimed by rewriting what is live, and a rewrite is not a compaction of
//!   on-disk generations.
//! - **No snapshot across a scan.** An iterator re-locks the store per item; a
//!   concurrent write can change what the next item is. That is the contract
//!   the remote backend states for a paged scan (ADR-0021): each item is a
//!   point answer under the lock, never a torn key/value pair, and never a
//!   frozen view of the range.
//! - **No cross-process access, no changelog capture.** The store is a value in
//!   the host's memory; `ChangelogStore` (ADR-0045) is a decorator nothing
//!   installs around it, because a store that writes nothing durable is not a
//!   system of record.
//!
//! What it DOES owe the rest of the crate is the whole ordinary contract: byte
//! order, read-your-writes, one atomic `commit_batch`, `shared_handle` = the
//! same keyspace, and the error surface (a scan never reports "failed" as
//! "empty"). Two obligations it owes its host and a disk engine does not:
//! creation is O(1) and drop returns every byte (no process-wide pool — every
//! table below belongs to this store), and every allocation is charged to the
//! store so a host can bound it.
//!
//! # Shape
//!
//! A packed B+tree over the FULL key. Leaves hold 32 slots; inner nodes hold 64
//! `(separator, child)` pairs, the separator being the SMALLEST key of a child's
//! subtree — so appending a key, which is the shape of okm's own keys, never
//! touches a separator.
//!
//! **A leaf stores the prefix its keys share once.** Sorted keys inside 32
//! adjacent entries share 8–15 bytes of okm's 11–19 byte keys — the `[ns][slot]`
//! head, the index field bytes, and the high bytes of a monotonic id — so the
//! leaf keeps that prefix in one record and each slot keeps only its own tail
//! (ADR-0072 §Decision 7). A key is `prefix ++ tail`, compared as two parts
//! without being joined, except when a scan hands it out as an owned `Vec`.
//!
//! The prefix is an invariant, not a heuristic: it is always a prefix of every
//! key in the leaf. A fresh leaf takes its FIRST key as the prefix and the
//! prefix only ever loosens — a key that diverges inside it shrinks it and
//! rewrites that leaf's records, which on monotonic keys happens at a byte
//! boundary rather than per insert. A split copies slots under the same prefix
//! into both halves, so it re-encodes nothing; re-encoding there instead would
//! make every split waste as much as the store holds live and turn the rebuild
//! threshold into a rewrite loop (ADR-0072 §Decision 7).
//!
//! Separators are full-key records of their own (one per 64 children), because
//! the entry they would otherwise point at no longer holds a whole key.
//!
//! Every byte lives in chunked tables: the records, the leaves and the inner
//! nodes, addressed by `u32` index. Nothing doubles, so a `drop` frees whole
//! chunks rather than one allocation per entry, and the growth slack is under
//! one chunk instead of under 2× the live bytes.
//!
//! Reads take a read lock and writes a write lock: computations read far more
//! than they write, so a scan does not queue behind another scan.
//!
//! Why this and not a prefix tree or a library: the measured win on this key
//! space is the PACKING — a `BTreeMap<Vec<u8>, Vec<u8>>` pays a `Vec` header
//! and a malloc header per key and per value, ~93–104 B of overhead on an entry
//! whose key is 11–19 B, and every generic `K`/`V` container reproduces that
//! because it owns `K` and `V` as Rust values. A packed B+tree with per-leaf
//! front coding lands at 25–40 B per index entry against 109 B, and its point
//! reads are within 1.05–1.3× of the `BTreeMap`'s (ADR-0072 §Context, §Measured).

use std::cmp::Ordering;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::engine::storage::{ScanIter, SharedVirtualStorage, VirtualStorage};

/// No node here.
const NIL: u32 = u32::MAX;
/// Entries per leaf, and the floor below which a leaf is refilled or merged.
const LEAF_SLOTS: usize = 32;
const LEAF_MIN: usize = LEAF_SLOTS / 2;
/// Children per inner node, and its floor.
const INNER_SLOTS: usize = 64;
const INNER_MIN: usize = INNER_SLOTS / 2;
/// Deeper than this and the store is beyond any realm; the descent uses a
/// fixed path array, so the bound is explicit.
const MAX_HEIGHT: usize = 12;
/// A tree node id is `slab_index << 1 | kind`.
const TAG_INNER: u32 = 1;

/// Records live in 64 KiB chunks, never spanning one; an address is
/// `(chunk << CHUNK_BITS) | offset` in a `u32`, so the store is capped at
/// 65 536 chunks = 4 GiB. A chunk doubles nothing: the slack a growing store
/// carries is under one chunk, which is what keeps a document-heavy store from
/// paying `Vec` doubling on top of its payload.
const CHUNK_BITS: u32 = 16;
const CHUNK: usize = 1 << CHUNK_BITS;
/// Records addressed by `u32`: the `(chunk, offset)` space is the limit.
const MAX_ARENA: usize = u32::MAX as usize;
/// Below this the arena is not worth rewriting.
const MIN_REBUILD: usize = 64 * 1024;

/// The longest common prefix of two keys.
fn lcp(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Compare an entry — `prefix ++ tail` — with `key`.
fn cmp_entry(prefix: &[u8], tail: &[u8], key: &[u8]) -> Ordering {
    let n = key.len().min(prefix.len());
    match prefix[..n].cmp(&key[..n]) {
        Ordering::Equal => {
            if key.len() < prefix.len() {
                // `key` is a proper prefix of this entry's key.
                Ordering::Greater
            } else {
                tail.cmp(&key[prefix.len()..])
            }
        }
        other => other,
    }
}

/// Fixed-size chunks of `T`, addressed by a `u32` index: `(chunk << bits) | off`.
///
/// One idea, used for the leaf table, the inner-node table and (in its own
/// form) the record bytes, so that **no `Vec` in the store doubles**. A `Vec`
/// that grows by doubling holds up to twice its live bytes at rest, which on
/// 40 000 index entries is ~16 B/entry — the difference between 45 and 62
/// B/entry, i.e. most of what the packing was for. A chunked table's slack is
/// one chunk.
struct Slab<T> {
    chunks: Vec<Vec<T>>,
    per_bits: u32,
}

impl<T: Copy> Slab<T> {
    fn new(per_bits: u32) -> Self {
        Self {
            chunks: Vec::new(),
            per_bits,
        }
    }

    fn per(&self) -> usize {
        1 << self.per_bits
    }

    fn mask(&self) -> u32 {
        (1 << self.per_bits) - 1
    }

    fn get(&self, id: u32) -> &T {
        &self.chunks[(id >> self.per_bits) as usize][(id & self.mask()) as usize]
    }

    fn get_mut(&mut self, id: u32) -> &mut T {
        let chunk = (id >> self.per_bits) as usize;
        let off = (id & self.mask()) as usize;
        &mut self.chunks[chunk][off]
    }

    /// Append one value; `None` once the `u32` index space (or the tree id
    /// space, which leaves the top bit free for the kind tag) is full.
    fn push(&mut self, value: T) -> Option<u32> {
        let per = self.per();
        let needs_new = match self.chunks.last() {
            None => true,
            Some(last) => last.len() == per,
        };
        if needs_new {
            if self.chunks.len() >= (1 << 20) {
                return None;
            }
            self.chunks.push(Vec::with_capacity(per));
        }
        let chunk = self.chunks.len() - 1;
        let chunk_vec = &mut self.chunks[chunk];
        let off = chunk_vec.len();
        chunk_vec.push(value);
        let id = u32::try_from((chunk << self.per_bits) | off).ok()?;
        // Leaves the top bit for the tree's node-kind tag.
        if id >= (1 << 31) {
            return None;
        }
        Some(id)
    }

    fn capacity_bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|c| c.capacity() * std::mem::size_of::<T>())
            .sum()
    }
}

/// One entry in a leaf: where its `[tail][value]` record starts. The key is
/// `leaf.prefix ++ tail`. 12 bytes.
#[derive(Clone, Copy, Default)]
struct Slot {
    rec_off: u32,
    suf_len: u32,
    val_len: u32,
}

/// A leaf: a sorted run of slots, the prefix the run's keys share (a prefix of
/// every one of them, `key = prefix ++ slot tail`), and the in-order links a
/// scan walks.
#[derive(Clone, Copy)]
struct Leaf {
    n: u32,
    prev: u32,
    next: u32,
    prefix_off: u32,
    prefix_len: u32,
    slots: [Slot; LEAF_SLOTS],
}

impl Default for Leaf {
    fn default() -> Self {
        Self {
            n: 0,
            prev: NIL,
            next: NIL,
            prefix_off: 0,
            prefix_len: 0,
            slots: [Slot::default(); LEAF_SLOTS],
        }
    }
}

/// A separator: the smallest key of a child's subtree, as its OWN record in the
/// arena — an entry's record holds only a tail, so a separator cannot borrow it.
#[derive(Clone, Copy, Default)]
struct Sep {
    rec_off: u32,
    key_len: u32,
}

/// An inner node: `seps[i]` is the smallest key under `children[i]`.
#[derive(Clone, Copy)]
struct Branch {
    n: u32,
    seps: [Sep; INNER_SLOTS],
    children: [u32; INNER_SLOTS],
}

impl Default for Branch {
    fn default() -> Self {
        Self {
            n: 0,
            seps: [Sep::default(); INNER_SLOTS],
            children: [NIL; INNER_SLOTS],
        }
    }
}

struct Inner {
    /// `[bytes][value]` records, bump-allocated across fixed chunks.
    chunks: Vec<Vec<u8>>,
    /// Index of the chunk currently being filled.
    open: usize,
    /// Record bytes that belong to deleted, replaced or re-encoded entries.
    waste: usize,
    /// Leaf table, chunked.
    leaves: Slab<Leaf>,
    /// Inner-node table, chunked.
    branches: Slab<Branch>,
    /// Freed node slots, reused before the tables grow.
    free_leaves: Vec<u32>,
    free_branches: Vec<u32>,
    /// Root node id (tagged) and height (1 = the root is a leaf).
    root: u32,
    height: u32,
    /// Bumped by every write. A scan cursor holds the epoch it was taken at:
    /// as long as the epoch matches, the cursor is a position and the next item
    /// is one slot; after any write it is discarded and the iterator
    /// re-descends from the key it last handed out.
    epoch: u64,
    len: usize,
}

fn leaf_tag(id: u32) -> u32 {
    id << 1
}

fn branch_tag(id: u32) -> u32 {
    (id << 1) | TAG_INNER
}

fn untag(id: u32) -> u32 {
    id >> 1
}

impl Inner {
    fn new() -> Self {
        let mut leaves = Slab::new(6); // 64 x 404 B = 26 KiB a chunk
        let branches = Slab::new(4); // 16 x 776 B = 12 KiB a chunk
        let root = leaves.push(Leaf::default()).map(leaf_tag).unwrap_or(NIL);
        Self {
            chunks: vec![Vec::with_capacity(CHUNK)],
            open: 0,
            waste: 0,
            leaves,
            branches,
            free_leaves: Vec::new(),
            free_branches: Vec::new(),
            root,
            height: 1,
            epoch: 1,
            len: 0,
        }
    }

    /// A record's bytes. A record never spans a chunk, so this is one slice and
    /// no join.
    fn record(&self, addr: u32, len: u32) -> &[u8] {
        // An empty record's address may sit one past the end of its chunk (the
        // end of a tail that is itself at the chunk's edge); it reads as empty
        // without consulting a chunk that may not exist yet.
        if len == 0 {
            return &[];
        }
        let chunk = (addr >> CHUNK_BITS) as usize;
        let off = (addr & (CHUNK as u32 - 1)) as usize;
        debug_assert!(
            chunk < self.chunks.len() && off + len as usize <= self.chunks[chunk].len(),
            "bad record addr {addr} len {len} (chunks {}, open {})",
            self.chunks.len(),
            self.open
        );
        &self.chunks[chunk][off..off + len as usize]
    }

    fn prefix(&self, leaf: &Leaf) -> &[u8] {
        self.record(leaf.prefix_off, leaf.prefix_len)
    }

    fn suffix(&self, slot: Slot) -> &[u8] {
        self.record(slot.rec_off, slot.suf_len)
    }

    fn value(&self, slot: Slot) -> &[u8] {
        self.record(slot.rec_off + slot.suf_len, slot.val_len)
    }

    fn sep_key(&self, sep: Sep) -> &[u8] {
        self.record(sep.rec_off, sep.key_len)
    }

    fn leaf(&self, id: u32) -> &Leaf {
        self.leaves.get(untag(id))
    }

    fn leaf_mut(&mut self, id: u32) -> &mut Leaf {
        self.leaves.get_mut(untag(id))
    }

    fn branch(&self, id: u32) -> &Branch {
        self.branches.get(untag(id))
    }

    fn branch_mut(&mut self, id: u32) -> &mut Branch {
        self.branches.get_mut(untag(id))
    }

    fn is_branch(id: u32) -> bool {
        id & TAG_INNER == TAG_INNER
    }

    /// A slot's whole key, joined. Only a caller that must HAND OUT the key
    /// pays for this; comparisons read it as two parts.
    fn key_of(&self, leaf_id: u32, slot: Slot) -> Vec<u8> {
        let leaf = self.leaf(leaf_id);
        let prefix = self.prefix(leaf);
        let tail = self.suffix(slot);
        let mut key = Vec::with_capacity(prefix.len() + tail.len());
        key.extend_from_slice(prefix);
        key.extend_from_slice(tail);
        key
    }

    /// Where `key` sits among a leaf's slots.
    fn leaf_pos(&self, leaf_id: u32, key: &[u8]) -> Result<usize, usize> {
        let leaf = self.leaf(leaf_id);
        let prefix = self.prefix(leaf);
        leaf.slots[..leaf.n as usize]
            .binary_search_by(|slot| cmp_entry(prefix, self.suffix(*slot), key))
    }

    /// The child whose key range holds `key`: the last separator that is `<=`.
    fn child_of(&self, node: &Branch, key: &[u8]) -> usize {
        let n = node.n as usize;
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.sep_key(node.seps[mid]) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.saturating_sub(1)
    }

    /// The leaf that owns `key`, with the branch path above it.
    fn descend(&self, key: &[u8]) -> (u32, [(u32, usize); MAX_HEIGHT], usize) {
        let mut path = [(NIL, 0usize); MAX_HEIGHT];
        let mut depth = 0usize;
        let mut id = self.root;
        while Self::is_branch(id) {
            let node = self.branch(id);
            let idx = self.child_of(node, key);
            if depth < MAX_HEIGHT {
                path[depth] = (id, idx);
                depth += 1;
            }
            id = node.children[idx];
        }
        (id, path, depth)
    }

    /// Append `[bytes]` and answer its offset. `None` means the store would
    /// cross the `u32` reference limit — a store of that size is a deployment
    /// error, and the caller reports it rather than truncating.
    fn append(&mut self, head: &[u8], tail: &[u8]) -> Option<u32> {
        let total = head.len() + tail.len();
        if total > MAX_ARENA {
            return None;
        }
        // A zero-length record has no bytes to place; address 0 is a valid empty
        // slice and never depends on where the open chunk ends. (Without this a
        // record appended to an exactly full chunk produced `open << 16 | CHUNK`,
        // which walks into the next chunk's index space.)
        if total == 0 {
            return Some(0);
        }
        if total > CHUNK {
            // One oversized record owns a chunk of its own (offset 0). The
            // model layer splits big values into their own table (ADR-0036),
            // so this is the guard, not the common path.
            if self.chunks.len() >= (MAX_ARENA >> CHUNK_BITS) {
                return None;
            }
            let mut chunk = Vec::with_capacity(total);
            chunk.extend_from_slice(head);
            chunk.extend_from_slice(tail);
            self.chunks.push(chunk);
            let addr = ((self.chunks.len() as u32 - 1) << CHUNK_BITS) as usize;
            return u32::try_from(addr).ok();
        }
        if self.chunks[self.open].len() + total > CHUNK {
            if self.chunks.len() >= (MAX_ARENA >> CHUNK_BITS) {
                return None;
            }
            self.chunks.push(Vec::with_capacity(CHUNK));
            self.open = self.chunks.len() - 1;
        }
        let chunk = &mut self.chunks[self.open];
        let off = chunk.len();
        chunk.extend_from_slice(head);
        chunk.extend_from_slice(tail);
        u32::try_from((self.open << CHUNK_BITS) | off).ok()
    }

    /// Write an entry's `[tail][value]` record. The caller guarantees `tail` is
    /// `key` minus the leaf's prefix.
    fn encode(&mut self, tail: &[u8], value: &[u8]) -> crate::error::Result<Slot> {
        let rec_off = self.append(tail, value).ok_or_else(too_big)?;
        Ok(Slot {
            rec_off,
            suf_len: tail.len() as u32,
            val_len: value.len() as u32,
        })
    }

    fn alloc_leaf(&mut self, leaf: Leaf) -> crate::error::Result<u32> {
        match self.free_leaves.pop() {
            Some(id) => {
                *self.leaves.get_mut(id) = leaf;
                Ok(leaf_tag(id))
            }
            None => self.leaves.push(leaf).map(leaf_tag).ok_or_else(too_big),
        }
    }

    fn alloc_branch(&mut self, node: Branch) -> crate::error::Result<u32> {
        match self.free_branches.pop() {
            Some(id) => {
                *self.branches.get_mut(id) = node;
                Ok(branch_tag(id))
            }
            None => self.branches.push(node).map(branch_tag).ok_or_else(too_big),
        }
    }

    fn free_leaf(&mut self, id: u32) {
        self.free_leaves.push(untag(id));
    }

    fn free_branch(&mut self, id: u32) {
        self.free_branches.push(untag(id));
    }

    /// A leaf's entries as owned pairs, in order.
    fn leaf_entries(&self, leaf_id: u32) -> Vec<(Vec<u8>, Vec<u8>)> {
        let leaf = self.leaf(leaf_id);
        let mut out = Vec::with_capacity(leaf.n as usize);
        for slot in &leaf.slots[..leaf.n as usize] {
            out.push((self.key_of(leaf_id, *slot), self.value(*slot).to_vec()));
        }
        out
    }

    /// Rewrite a leaf's whole run: recompute the shared prefix from its ends,
    /// store it once, and encode each entry against it. Every structural change
    /// (insert that does not fit the prefix, split, borrow, merge) goes through
    /// here, so one function defines the encoding.
    fn reset_leaf(
        &mut self,
        leaf_id: u32,
        entries: &[(Vec<u8>, Vec<u8>)],
    ) -> crate::error::Result<()> {
        debug_assert!(entries.len() <= LEAF_SLOTS);
        if entries.is_empty() {
            let leaf = *self.leaf(leaf_id);
            self.waste += leaf.prefix_len as usize;
            for slot in &leaf.slots[..leaf.n as usize] {
                self.waste += (slot.suf_len + slot.val_len) as usize;
            }
            let leaf = self.leaf_mut(leaf_id);
            leaf.n = 0;
            leaf.prefix_off = 0;
            leaf.prefix_len = 0;
            return Ok(());
        }
        let shared = match (entries.first(), entries.last()) {
            (Some((first, _)), Some((last, _))) => lcp(first, last),
            _ => 0,
        };
        // The old run's bytes, prefix included, become waste.
        {
            let leaf = *self.leaf(leaf_id);
            self.waste += leaf.prefix_len as usize;
            for slot in &leaf.slots[..leaf.n as usize] {
                self.waste += (slot.suf_len + slot.val_len) as usize;
            }
        }
        let prefix_off = self
            .append(&entries[0].0[..shared], &[])
            .ok_or_else(too_big)?;
        let mut slots = [Slot::default(); LEAF_SLOTS];
        for (at, (key, value)) in entries.iter().enumerate() {
            slots[at] = self.encode(&key[shared..], value)?;
        }
        let leaf = self.leaf_mut(leaf_id);
        leaf.prefix_off = prefix_off;
        leaf.prefix_len = shared as u32;
        leaf.slots[..entries.len()].copy_from_slice(&slots[..entries.len()]);
        leaf.n = entries.len() as u32;
        Ok(())
    }

    /// A leaf's smallest key, materialised as a separator record.
    fn sep_of(&mut self, leaf_id: u32) -> crate::error::Result<Sep> {
        let slot = self.leaf(leaf_id).slots[0];
        let key = self.key_of(leaf_id, slot);
        let rec_off = self.append(&key, &[]).ok_or_else(too_big)?;
        Ok(Sep {
            rec_off,
            key_len: key.len() as u32,
        })
    }

    fn insert(&mut self, key: &[u8], value: &[u8]) -> crate::error::Result<()> {
        let (leaf_id, path, depth) = self.descend(key);
        match self.leaf_pos(leaf_id, key) {
            Ok(at) => {
                // Replace: the key is unchanged, so only a new record is needed.
                let leaf = *self.leaf(leaf_id);
                let old = leaf.slots[at];
                self.waste += (old.suf_len + old.val_len) as usize;
                let shared = leaf.prefix_len as usize;
                let slot = self.encode(&key[shared..], value)?;
                self.leaf_mut(leaf_id).slots[at] = slot;
            }
            Err(at) => {
                let leaf = *self.leaf(leaf_id);
                let n = leaf.n as usize;
                if n == 0 {
                    // A fresh leaf takes its first key as the shared prefix, so
                    // its tail is empty and every later key in this leaf shares
                    // at least that much. Compression therefore starts at 100%
                    // and only ever loosens — no rewrite is needed to ACQUIRE a
                    // prefix, which is what keeps the split path waste-free.
                    let prefix_off = self.append(key, &[]).ok_or_else(too_big)?;
                    let slot = self.encode(&[], value)?;
                    {
                        let leaf = self.leaf_mut(leaf_id);
                        leaf.prefix_off = prefix_off;
                        leaf.prefix_len = key.len() as u32;
                        leaf.slots[0] = slot;
                        leaf.n = 1;
                    }
                    self.len += 1;
                    if depth > 0 {
                        // An emptied non-root leaf keeps its parent's separator
                        // pointing at a key that no longer exists until this
                        // write gives it one back.
                        let sep = self.sep_of(leaf_id)?;
                        self.lift_min(&path, depth, sep);
                    }
                    self.epoch += 1;
                    return Ok(());
                }
                let fits = key.starts_with(self.record(leaf.prefix_off, leaf.prefix_len));
                if !fits {
                    // The new key diverges inside the shared prefix, so the
                    // prefix must shrink: every entry's tail grows and every
                    // record is rewritten. Correctly routed keys share the
                    // prefix of their leaf's ends, so this is the byte-boundary
                    // case, not the common one.
                    let shorter = lcp(self.record(leaf.prefix_off, leaf.prefix_len), key);
                    let prefix = self.record(leaf.prefix_off, shorter as u32).to_vec();
                    self.shorten_prefix(leaf_id, &prefix)?;
                }
                let shared = self.leaf(leaf_id).prefix_len as usize;
                let slot = self.encode(&key[shared..], value)?;
                self.len += 1;
                if n < LEAF_SLOTS {
                    let leaf = self.leaf_mut(leaf_id);
                    leaf.slots.copy_within(at..n, at + 1);
                    leaf.slots[at] = slot;
                    leaf.n += 1;
                } else {
                    self.split_slots(leaf_id, at, slot, &path, depth)?;
                }
                if at == 0 {
                    // This child's smallest key moved, so every separator naming
                    // it moves too.
                    let sep = self.sep_of(leaf_id)?;
                    self.lift_min(&path, depth, sep);
                }
            }
        }
        self.epoch += 1;
        Ok(())
    }

    /// Rewrite `seps[idx]` from the leaf up, stopping at the first level whose
    /// child is not its parent's first (a later separator does not change its
    /// parent's own minimum).
    fn lift_min(&mut self, path: &[(u32, usize); MAX_HEIGHT], depth: usize, sep: Sep) {
        for d in (0..depth).rev() {
            let (parent, idx) = path[d];
            self.branch_mut(parent).seps[idx] = sep;
            if idx != 0 {
                return;
            }
        }
    }

    /// Split `LEAF_SLOTS` slots plus the new one into the existing leaf and a
    /// new right sibling, then push the new separator up.
    ///
    /// The split point is biased when the new key lands at one END of the run:
    /// okm's own keys are monotonic (ids, timestamps, uuid7), and an even split
    /// would leave every finished leaf half empty forever — the measured
    /// difference is 43 B/entry against 31.
    ///
    /// Both halves keep the parent leaf's prefix, so NO slot is re-encoded: a
    /// slot's record is `key[prefix.len()..]`, which stays valid under the same
    /// prefix. (Re-encoding here would make every split manufacture as much
    /// waste as the store holds live, and the rebuild would then thrash.)
    fn split_slots(
        &mut self,
        leaf_id: u32,
        at: usize,
        slot: Slot,
        path: &[(u32, usize); MAX_HEIGHT],
        depth: usize,
    ) -> crate::error::Result<()> {
        let total = LEAF_SLOTS + 1;
        let half = if at + 1 == total {
            total - 1
        } else if at == 0 {
            1
        } else {
            total / 2
        };
        let mut slots = [Slot::default(); LEAF_SLOTS + 1];
        let (prefix_off, prefix_len, old_next) = {
            let leaf = self.leaf(leaf_id);
            slots[..at].copy_from_slice(&leaf.slots[..at]);
            slots[at] = slot;
            slots[at + 1..total].copy_from_slice(&leaf.slots[at..LEAF_SLOTS]);
            (leaf.prefix_off, leaf.prefix_len, leaf.next)
        };
        let right_id = self.alloc_leaf(Leaf {
            prev: leaf_id,
            next: old_next,
            prefix_off,
            prefix_len,
            ..Default::default()
        })?;
        {
            let left = self.leaf_mut(leaf_id);
            left.slots[..half].copy_from_slice(&slots[..half]);
            left.n = half as u32;
            left.next = right_id;
        }
        {
            let right = self.leaf_mut(right_id);
            right.slots[..total - half].copy_from_slice(&slots[half..total]);
            right.n = (total - half) as u32;
        }
        if old_next != NIL {
            self.leaf_mut(old_next).prev = right_id;
        }
        let sep = self.sep_of(right_id)?;
        self.insert_sep(path, depth, sep, right_id)
    }

    /// Give a leaf a shorter shared prefix and re-encode every entry against
    /// it. The caller has established that `prefix` is a prefix of every key in
    /// the leaf.
    fn shorten_prefix(&mut self, leaf_id: u32, prefix: &[u8]) -> crate::error::Result<()> {
        let leaf = *self.leaf(leaf_id);
        let keep = prefix.len() as u32;
        let grow = self
            .record(leaf.prefix_off + keep, leaf.prefix_len - keep)
            .to_vec();
        let mut rewritten = Vec::with_capacity(leaf.n as usize);
        for old in leaf.slots[..leaf.n as usize].iter().copied() {
            let mut tail = Vec::with_capacity(grow.len() + old.suf_len as usize);
            tail.extend_from_slice(&grow);
            tail.extend_from_slice(self.suffix(old));
            rewritten.push((tail, self.value(old).to_vec()));
        }
        self.waste += leaf.prefix_len as usize;
        for (tail, value) in &rewritten {
            self.waste += tail.len() + value.len();
        }
        let prefix_off = self.append(prefix, &[]).ok_or_else(too_big)?;
        let mut slots = [Slot::default(); LEAF_SLOTS];
        for (at, (tail, value)) in rewritten.iter().enumerate() {
            slots[at] = self.encode(tail, value)?;
        }
        let leaf = self.leaf_mut(leaf_id);
        leaf.prefix_off = prefix_off;
        leaf.prefix_len = keep;
        leaf.slots[..rewritten.len()].copy_from_slice(&slots[..rewritten.len()]);
        Ok(())
    }

    /// Add `(sep, child)` to the parent at `path[depth-1]`, splitting upward
    /// when that parent is full. `depth == 0` means the split node was the root.
    fn insert_sep(
        &mut self,
        path: &[(u32, usize); MAX_HEIGHT],
        depth: usize,
        sep: Sep,
        child: u32,
    ) -> crate::error::Result<()> {
        if depth == 0 {
            let mut root = Branch {
                n: 2,
                ..Default::default()
            };
            root.seps[0] = self.node_min(self.root)?;
            root.children[0] = self.root;
            root.seps[1] = sep;
            root.children[1] = child;
            let id = self.alloc_branch(root)?;
            self.root = id;
            self.height += 1;
            return Ok(());
        }
        let (parent, idx) = path[depth - 1];
        let mut seps = [Sep::default(); INNER_SLOTS + 1];
        let mut children = [NIL; INNER_SLOTS + 1];
        let split = {
            let node = self.branch(parent);
            let n = node.n as usize;
            seps[..=idx].copy_from_slice(&node.seps[..=idx]);
            children[..=idx].copy_from_slice(&node.children[..=idx]);
            seps[idx + 1] = sep;
            children[idx + 1] = child;
            seps[idx + 2..n + 1].copy_from_slice(&node.seps[idx + 1..n]);
            children[idx + 2..n + 1].copy_from_slice(&node.children[idx + 1..n]);
            let total = n + 1;
            if total <= INNER_SLOTS {
                let node = self.branch_mut(parent);
                node.n = total as u32;
                node.seps[..total].copy_from_slice(&seps[..total]);
                node.children[..total].copy_from_slice(&children[..total]);
                None
            } else {
                // Full: the right half becomes a sibling (biased when the new
                // separator lands at an end, for the same reason as leaves).
                let half = if idx + 1 == total - 1 {
                    total - 1
                } else if idx + 1 == 0 {
                    1
                } else {
                    total / 2
                };
                {
                    let node = self.branch_mut(parent);
                    node.n = half as u32;
                    node.seps[..half].copy_from_slice(&seps[..half]);
                    node.children[..half].copy_from_slice(&children[..half]);
                }
                let mut right = Branch {
                    n: (total - half) as u32,
                    ..Default::default()
                };
                right.seps[..total - half].copy_from_slice(&seps[half..total]);
                right.children[..total - half].copy_from_slice(&children[half..total]);
                let first = right.seps[0];
                Some((first, self.alloc_branch(right)?))
            }
        };
        match split {
            Some((sep, right)) => self.insert_sep(path, depth - 1, sep, right),
            None => Ok(()),
        }
    }

    /// The smallest key under a node.
    fn node_min(&mut self, id: u32) -> crate::error::Result<Sep> {
        if Self::is_branch(id) {
            Ok(self.branch(id).seps[0])
        } else {
            self.sep_of(id)
        }
    }

    fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let (leaf_id, _, _) = self.descend(key);
        match self.leaf_pos(leaf_id, key) {
            Ok(at) => Some(self.value(self.leaf(leaf_id).slots[at])),
            Err(_) => None,
        }
    }

    fn remove(&mut self, key: &[u8]) -> bool {
        let (leaf_id, path, depth) = self.descend(key);
        let at = match self.leaf_pos(leaf_id, key) {
            Ok(at) => at,
            Err(_) => return false,
        };
        let leaf = *self.leaf(leaf_id);
        let n = leaf.n as usize;
        let gone = leaf.slots[at];
        self.waste += (gone.suf_len + gone.val_len) as usize;
        {
            let l = self.leaf_mut(leaf_id);
            l.slots.copy_within(at + 1..n, at);
            l.n -= 1;
        }
        self.len -= 1;
        self.epoch += 1;
        if at == 0 && self.leaf(leaf_id).n > 0 {
            // `sep_of` borrows `self` mutably, so the min is taken afterwards.
            if let Ok(sep) = self.sep_of(leaf_id) {
                self.lift_min(&path, depth, sep);
            }
        }
        if self.height > 1 && (self.leaf(leaf_id).n as usize) < LEAF_MIN {
            self.fix_leaf(leaf_id, &path, depth);
        }
        self.collapse_root();
        true
    }

    /// A root with a single child becomes that child, one level shorter.
    fn collapse_root(&mut self) {
        while self.height > 1 {
            let root = self.root;
            let n = self.branch(root).n;
            if n != 1 {
                return;
            }
            let only = self.branch(root).children[0];
            self.root = only;
            self.height -= 1;
            self.free_branch(root);
        }
    }

    /// Refill a leaf that dropped below the floor: borrow from an adjacent
    /// sibling with room, otherwise merge, then repair the parent. Only
    /// siblings under the SAME parent qualify — the in-order links also cross
    /// parents, and merging across one would orphan a child entry.
    fn fix_leaf(&mut self, leaf_id: u32, path: &[(u32, usize); MAX_HEIGHT], depth: usize) {
        let (parent, idx) = path[depth - 1];
        let n = self.branch(parent).n as usize;
        let right = if idx + 1 < n {
            Some(self.branch(parent).children[idx + 1])
        } else {
            None
        };
        let left = if idx > 0 {
            Some(self.branch(parent).children[idx - 1])
        } else {
            None
        };
        if let Some(right) = right.filter(|r| self.leaf(*r).n as usize > LEAF_MIN) {
            let mut mine = self.leaf_entries(leaf_id);
            let mut theirs = self.leaf_entries(right);
            mine.push(theirs.remove(0));
            if self.reset_leaf(leaf_id, &mine).is_err() || self.reset_leaf(right, &theirs).is_err()
            {
                return;
            }
            if let Ok(sep) = self.sep_of(right) {
                self.branch_mut(parent).seps[idx + 1] = sep;
            }
            return;
        }
        if let Some(left) = left.filter(|l| self.leaf(*l).n as usize > LEAF_MIN) {
            let mut mine = self.leaf_entries(leaf_id);
            let mut theirs = self.leaf_entries(left);
            let moved = theirs.pop();
            let Some(moved) = moved else { return };
            mine.insert(0, moved);
            if self.reset_leaf(leaf_id, &mine).is_err() || self.reset_leaf(left, &theirs).is_err() {
                return;
            }
            if let Ok(sep) = self.sep_of(leaf_id) {
                self.lift_min(path, depth, sep);
            }
            return;
        }
        if let Some(right) = right {
            let mut mine = self.leaf_entries(leaf_id);
            mine.extend(self.leaf_entries(right));
            let next = self.leaf(right).next;
            if self.reset_leaf(leaf_id, &mine).is_err() {
                return;
            }
            {
                let l = self.leaf_mut(leaf_id);
                l.next = next;
            }
            if next != NIL {
                self.leaf_mut(next).prev = leaf_id;
            }
            self.free_leaf(right);
            self.drop_child(parent, idx + 1, path, depth);
        } else if let Some(left) = left {
            let mut mine = self.leaf_entries(left);
            mine.extend(self.leaf_entries(leaf_id));
            let next = self.leaf(leaf_id).next;
            if self.reset_leaf(left, &mine).is_err() {
                return;
            }
            self.leaf_mut(left).next = next;
            if next != NIL {
                self.leaf_mut(next).prev = left;
            }
            self.free_leaf(leaf_id);
            self.drop_child(parent, idx, path, depth);
        }
    }

    /// Remove `idx` from a parent, then repair the parent if it dropped below
    /// its floor, and move the grandparent's separator when the parent's own
    /// smallest key changed.
    fn drop_child(
        &mut self,
        parent: u32,
        idx: usize,
        path: &[(u32, usize); MAX_HEIGHT],
        depth: usize,
    ) {
        {
            let node = self.branch_mut(parent);
            let n = node.n as usize;
            node.seps.copy_within(idx + 1..n, idx);
            node.children.copy_within(idx + 1..n, idx);
            node.n -= 1;
        }
        if idx == 0 && self.branch(parent).n > 0 && depth >= 2 {
            let min = self.branch(parent).seps[0];
            self.lift_min(path, depth - 1, min);
        }
        if depth >= 2 && (self.branch(parent).n as usize) < INNER_MIN {
            self.fix_branch(parent, path, depth - 1);
        }
    }

    fn fix_branch(&mut self, id: u32, path: &[(u32, usize); MAX_HEIGHT], depth: usize) {
        let (parent, idx) = path[depth - 1];
        let n = self.branch(parent).n as usize;
        if idx + 1 < n {
            let right = self.branch(parent).children[idx + 1];
            let (rn, mn) = (self.branch(right).n as usize, self.branch(id).n as usize);
            if mn + rn <= INNER_SLOTS {
                let rseps = self.branch(right).seps;
                let rchildren = self.branch(right).children;
                {
                    let l = self.branch_mut(id);
                    l.seps[mn..mn + rn].copy_from_slice(&rseps[..rn]);
                    l.children[mn..mn + rn].copy_from_slice(&rchildren[..rn]);
                    l.n += rn as u32;
                }
                self.free_branch(right);
                self.drop_child(parent, idx + 1, path, depth);
            } else {
                // Borrow one child: the pair does not fit in one node.
                let (sep, child) = {
                    let r = self.branch(right);
                    (r.seps[0], r.children[0])
                };
                {
                    let r = self.branch_mut(right);
                    let total = r.n as usize;
                    r.seps.copy_within(1..total, 0);
                    r.children.copy_within(1..total, 0);
                    r.n -= 1;
                }
                {
                    let l = self.branch_mut(id);
                    l.seps[mn] = sep;
                    l.children[mn] = child;
                    l.n += 1;
                }
                let min = self.branch(right).seps[0];
                self.branch_mut(parent).seps[idx + 1] = min;
            }
        } else if idx > 0 {
            let left = self.branch(parent).children[idx - 1];
            let (ln, mn) = (self.branch(left).n as usize, self.branch(id).n as usize);
            if mn + ln <= INNER_SLOTS {
                let mseps = self.branch(id).seps;
                let mchildren = self.branch(id).children;
                let base = ln;
                {
                    let l = self.branch_mut(left);
                    l.seps[base..base + mn].copy_from_slice(&mseps[..mn]);
                    l.children[base..base + mn].copy_from_slice(&mchildren[..mn]);
                    l.n += mn as u32;
                }
                self.free_branch(id);
                self.drop_child(parent, idx, path, depth);
            } else {
                let (sep, child) = {
                    let l = self.branch(left);
                    let last = l.n as usize - 1;
                    (l.seps[last], l.children[last])
                };
                self.branch_mut(left).n -= 1;
                {
                    let l = self.branch_mut(id);
                    l.seps.copy_within(0..mn, 1);
                    l.children.copy_within(0..mn, 1);
                    l.seps[0] = sep;
                    l.children[0] = child;
                    l.n += 1;
                }
                self.lift_min(path, depth, sep);
            }
        }
    }

    /// First slot at or after `key`.
    fn seek_ge(&self, key: &[u8]) -> Option<(u32, usize)> {
        let (mut leaf_id, _, _) = self.descend(key);
        while leaf_id != NIL {
            let leaf = self.leaf(leaf_id);
            match self.leaf_pos(leaf_id, key) {
                Ok(at) => return Some((leaf_id, at)),
                Err(at) if at < leaf.n as usize => return Some((leaf_id, at)),
                Err(_) => leaf_id = leaf.next,
            }
        }
        None
    }

    /// First slot strictly after `key`.
    fn seek_gt(&self, key: &[u8]) -> Option<(u32, usize)> {
        let (leaf_id, at) = self.seek_ge(key)?;
        let leaf = self.leaf(leaf_id);
        if at < leaf.n as usize
            && cmp_entry(self.prefix(leaf), self.suffix(leaf.slots[at]), key) == Ordering::Equal
        {
            if at + 1 < leaf.n as usize {
                return Some((leaf_id, at + 1));
            }
            if leaf.next == NIL {
                return None;
            }
            return Some((leaf.next, 0));
        }
        Some((leaf_id, at))
    }

    /// Where a backward walk starts for `key`: `(leaf, p)` means "emit slot
    /// `p - 1` next", so the first slot strictly before `key` is `p - 1`. A
    /// `p` of 0 hands the walk to the previous leaf, which the caller does.
    fn seek_lt(&self, key: &[u8]) -> Option<(u32, usize)> {
        let (leaf_id, _, _) = self.descend(key);
        let at = match self.leaf_pos(leaf_id, key) {
            Ok(at) | Err(at) => at,
        };
        Some((leaf_id, at))
    }

    fn last_slot(&self) -> Option<(u32, usize)> {
        let mut id = self.root;
        while Self::is_branch(id) {
            let node = self.branch(id);
            id = node.children[node.n as usize - 1];
        }
        let leaf = self.leaf(id);
        if leaf.n == 0 {
            None
        } else {
            // "Emit slot n - 1 next".
            Some((id, leaf.n as usize))
        }
    }

    fn first_leaf(&self) -> u32 {
        let mut id = self.root;
        while Self::is_branch(id) {
            id = self.branch(id).children[0];
        }
        id
    }

    /// Reclaim the wasted half of the records once it outweighs the live half.
    /// A rewrite, not a compaction: it rebuilds this store from its own live
    /// entries, in order, which also packs every leaf to the top.
    fn rebuild(&mut self) {
        let mut fresh = Inner::new();
        let mut complete = true;
        let mut leaf_id = self.first_leaf();
        'outer: while leaf_id != NIL {
            let leaf = self.leaf(leaf_id);
            for slot in leaf.slots[..leaf.n as usize].iter().copied() {
                let key = self.key_of(leaf_id, slot);
                let value = self.value(slot).to_vec();
                if fresh.insert(&key, &value).is_err() {
                    complete = false;
                    break 'outer;
                }
            }
            leaf_id = leaf.next;
        }
        if complete {
            fresh.epoch = self.epoch + 1;
            *self = fresh;
        }
    }

    fn live_bytes(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }

    fn maybe_rebuild(&mut self) {
        let held = self.live_bytes();
        if held >= MIN_REBUILD && self.waste * 2 > held {
            self.rebuild();
        }
    }
}

fn too_big() -> crate::error::Error {
    crate::error::Error::Engine(format!(
        "ephemeral store: a record or a node would cross the {} GiB reference limit",
        MAX_ARENA / (1 << 30)
    ))
}

/// The store behind a computation that does not persist — a temporary realm
/// being the common case.
///
/// Cloning shares the keyspace (one `Arc`), which is the handle semantics every
/// other backend has. Dropping the last clone returns every byte the store
/// allocated: a handful of chunked tables, none of them process-global.
#[derive(Clone)]
pub struct EphemeralStore {
    inner: Arc<RwLock<Inner>>,
}

impl Default for EphemeralStore {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner::new())),
        }
    }
}

impl EphemeralStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Live entries. Not part of the engine contract (no other backend answers
    /// it); it is here because the host reports its size.
    pub fn len(&self) -> usize {
        self.read().len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes this store holds, allocated and wasted, for a host that bounds it.
    pub fn bytes(&self) -> usize {
        let g = self.read();
        let chunks: usize = g.chunks.iter().map(Vec::capacity).sum();
        chunks + g.leaves.capacity_bytes() + g.branches.capacity_bytes()
    }
}

impl VirtualStorage for EphemeralStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()> {
        let mut g = self.write();
        g.insert(&key, &value)?;
        g.maybe_rebuild();
        Ok(())
    }

    fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>> {
        Ok(self.read().get(key).map(<[u8]>::to_vec))
    }

    fn del(&self, key: &[u8]) -> crate::error::Result<()> {
        let mut g = self.write();
        g.remove(key);
        g.maybe_rebuild();
        Ok(())
    }

    /// One write lock for the whole list: a concurrent reader sees none of it
    /// or all of it. The trait default would take the lock per op, which for
    /// this engine is a real difference — the ops are already in memory and the
    /// only cost is the guard.
    fn commit_batch(&self, batch: crate::engine::storage::Batch) -> crate::error::Result<()> {
        let mut g = self.write();
        for (key, value) in batch.ops {
            match value {
                Some(value) => g.insert(&key, &value)?,
                None => {
                    g.remove(&key);
                }
            }
        }
        g.maybe_rebuild();
        Ok(())
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        ScanIter::Ephemeral(EphemeralScan {
            store: Arc::clone(&self.inner),
            lo: begin.to_vec(),
            hi: end.map(<[u8]>::to_vec),
            fwd_from: Some(begin.to_vec()),
            fwd_last: None,
            bwd_last: None,
            fwd_cursor: None,
            bwd_cursor: None,
            fwd_done: false,
            bwd_done: false,
        })
    }
}

impl SharedVirtualStorage for EphemeralStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}

/// A cursor over an [`EphemeralStore`]'s range.
///
/// The item shape is the contract's (`Vec<u8>` pairs, errors riding the items),
/// so a scan copies what it yields — one entry at a time, never the range.
/// Consistency is stated in the module docs: each item is taken under the read
/// lock, and a write between items may change what comes next.
pub struct EphemeralScan {
    store: Arc<RwLock<Inner>>,
    lo: Vec<u8>,
    hi: Option<Vec<u8>>,
    /// Inclusive lower bound set by `seek` (or the range's begin) while no item
    /// has been handed out since; `None` means "continue from the cursor".
    fwd_from: Option<Vec<u8>>,
    /// Last key handed out forward; the re-seek base after an epoch change.
    fwd_last: Option<Vec<u8>>,
    /// Last key handed out backward.
    bwd_last: Option<Vec<u8>>,
    /// `(leaf, slot, epoch)` — a cached position valid only while the epoch
    /// holds.
    fwd_cursor: Option<(u32, u32, u64)>,
    bwd_cursor: Option<(u32, u32, u64)>,
    fwd_done: bool,
    bwd_done: bool,
}

impl EphemeralScan {
    /// Read through a CLONED `Arc`: the guard then borrows this call's local,
    /// so the caller may still mutate the scan's own fields afterwards.
    fn with<R>(&self, f: impl FnOnce(&Inner) -> R) -> R {
        let store = Arc::clone(&self.store);
        let guard = store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&guard)
    }

    /// The forward frontier as of the last pull, for `seek`'s "at or behind the
    /// current position is a no-op" rule.
    fn frontier(&self) -> &[u8] {
        match (&self.fwd_from, &self.fwd_last) {
            (Some(from), _) => from.as_slice(),
            (None, Some(last)) => last.as_slice(),
            (None, None) => self.lo.as_slice(),
        }
    }

    fn within(&self, key: &[u8]) -> bool {
        if self.hi.as_ref().is_some_and(|hi| key >= hi.as_slice()) {
            return false;
        }
        if self.bwd_last.as_ref().is_some_and(|b| key >= b.as_slice()) {
            return false;
        }
        true
    }

    /// Move the cursor to the first entry at or after `lower`, forward only.
    pub(crate) fn seek(&mut self, lower: &[u8]) -> bool {
        if self.fwd_done {
            return false;
        }
        // Forward-only, per the trait's rule: a lower at or behind the current
        // position keeps the position.
        if lower <= self.frontier() {
            return true;
        }
        let (found, epoch) = self.with(|g| (g.seek_ge(lower), g.epoch));
        let ok = match found {
            Some((leaf, slot)) => {
                self.with(|g| self.within(&g.key_of(leaf, g.leaf(leaf).slots[slot])))
            }
            None => false,
        };
        self.fwd_from = Some(lower.to_vec());
        self.fwd_last = None;
        self.fwd_cursor = match (ok, found) {
            (true, Some((leaf, slot))) => Some((leaf, slot as u32, epoch)),
            _ => None,
        };
        self.fwd_done = !ok;
        ok
    }
}

impl Iterator for EphemeralScan {
    type Item = crate::error::Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.fwd_done {
            return None;
        }
        let (position, key, val, epoch) = self.with(|g| {
            let mut position = match self.fwd_cursor {
                Some((leaf, slot, epoch)) if epoch == g.epoch => Some((leaf, slot as usize)),
                _ => match (&self.fwd_from, &self.fwd_last) {
                    (Some(from), _) => g.seek_ge(from),
                    (None, Some(last)) => g.seek_gt(last),
                    (None, None) => g.seek_ge(&self.lo),
                },
            };
            loop {
                match position {
                    Some((leaf, slot)) if slot < g.leaf(leaf).n as usize => break,
                    Some((leaf, _)) => {
                        let next = g.leaf(leaf).next;
                        position = if next == NIL { None } else { Some((next, 0)) };
                    }
                    None => break,
                }
            }
            match position {
                None => (None, Vec::new(), Vec::new(), g.epoch),
                Some((leaf, slot)) => {
                    let s = g.leaf(leaf).slots[slot];
                    (
                        Some((leaf, slot)),
                        g.key_of(leaf, s),
                        g.value(s).to_vec(),
                        g.epoch,
                    )
                }
            }
        });
        let (leaf, slot) = match position {
            Some(p) if self.within(&key) => p,
            _ => {
                self.fwd_done = true;
                return None;
            }
        };
        self.fwd_last = Some(key.clone());
        self.fwd_from = None;
        self.fwd_cursor = Some((leaf, slot as u32 + 1, epoch));
        Some(Ok((key, val)))
    }
}

impl DoubleEndedIterator for EphemeralScan {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.bwd_done {
            return None;
        }
        let (position, key, val, epoch) = self.with(|g| {
            let mut position = match self.bwd_cursor {
                Some((leaf, slot, epoch)) if epoch == g.epoch => Some((leaf, slot as usize)),
                _ => match (&self.bwd_last, &self.hi) {
                    (Some(last), _) => g.seek_lt(last),
                    (None, Some(hi)) => g.seek_lt(hi),
                    (None, None) => g.last_slot(),
                },
            };
            loop {
                match position {
                    Some((_, slot)) if slot > 0 => break,
                    Some((leaf, _)) => {
                        let prev = g.leaf(leaf).prev;
                        position = if prev == NIL {
                            None
                        } else {
                            Some((prev, g.leaf(prev).n as usize))
                        };
                    }
                    None => break,
                }
            }
            match position {
                None => (None, Vec::new(), Vec::new(), g.epoch),
                Some((leaf, slot)) => {
                    let s = g.leaf(leaf).slots[slot - 1];
                    (
                        Some((leaf, slot - 1)),
                        g.key_of(leaf, s),
                        g.value(s).to_vec(),
                        g.epoch,
                    )
                }
            }
        });
        let (leaf, slot) = match position {
            Some(p) if self.within(&key) => p,
            _ => {
                self.bwd_done = true;
                return None;
            }
        };
        if key.as_slice() < self.lo.as_slice() {
            self.bwd_done = true;
            return None;
        }
        if self
            .fwd_last
            .as_ref()
            .is_some_and(|f| key.as_slice() <= f.as_slice())
        {
            self.bwd_done = true;
            return None;
        }
        self.bwd_last = Some(key.clone());
        self.bwd_cursor = Some((leaf, slot as u32, epoch));
        Some(Ok((key, val)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// splitmix64, so a failing sequence is reproducible from its seed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn key(n: u64) -> Vec<u8> {
        let mut k = Vec::new();
        k.extend_from_slice(&3u16.to_be_bytes());
        k.push(0x00);
        k.extend_from_slice(&n.to_be_bytes());
        k
    }

    fn drain(store: &EphemeralStore, begin: &[u8], end: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        store
            .scan_range(begin, end)
            .collect::<crate::error::Result<Vec<_>>>()
            .expect("no faults in an in-memory scan")
    }

    #[test]
    fn a_key_that_is_a_prefix_of_another_is_its_own_entry() {
        let s = EphemeralStore::new();
        s.put(b"a".to_vec(), b"short".to_vec()).unwrap();
        s.put(b"ab".to_vec(), b"long".to_vec()).unwrap();
        s.put(b"abc".to_vec(), Vec::new()).unwrap();
        assert_eq!(s.get(b"a").unwrap().as_deref(), Some(&b"short"[..]));
        assert_eq!(s.get(b"ab").unwrap().as_deref(), Some(&b"long"[..]));
        assert_eq!(s.get(b"abc").unwrap().as_deref(), Some(&b""[..]));
        assert_eq!(s.len(), 3);
        let all = drain(&s, b"", None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, b"a");
        assert_eq!(all[2].0, b"abc");
    }

    /// An index-shaped key: `[ns][slot][org][id]`, 16 bytes, empty value.
    fn index_key(n: u64) -> Vec<u8> {
        let mut k = Vec::new();
        k.extend_from_slice(&3u16.to_be_bytes());
        k.extend_from_slice(&[0x40, 0x00]);
        k.extend_from_slice(&(n / 32_000).to_be_bytes().as_slice()[4..]);
        k.extend_from_slice(&n.to_be_bytes());
        k
    }

    #[test]
    fn a_leaf_stores_the_prefix_its_keys_share_once() {
        let s = EphemeralStore::new();
        for n in 0..32_000u64 {
            s.put(index_key(n), Vec::new()).unwrap();
        }
        // 12 B slot + the leaf table's amortised header, and then only the few
        // tail bytes a 32-key run does not share. Without the shared prefix the
        // arena would carry all 16 key bytes per entry, i.e. ~31 B/entry.
        let per_entry = s.bytes() / s.len();
        assert!(per_entry < 22, "{per_entry} B/entry");
        for n in [0u64, 1, 31, 32, 33, 1024, 31_999] {
            assert_eq!(s.get(&index_key(n)).unwrap(), Some(Vec::new()), "key {n}");
        }
        let all: Vec<Vec<u8>> = drain(&s, b"", None).into_iter().map(|(k, _)| k).collect();
        let want: Vec<Vec<u8>> = (0..32_000u64).map(index_key).collect();
        assert_eq!(all, want);
    }

    #[test]
    fn a_key_that_diverges_inside_the_prefix_shortens_it() {
        let s = EphemeralStore::new();
        // 64 keys sharing ten bytes, so the leaf's prefix covers most of them.
        for n in 0..64u64 {
            let mut k = b"meta.scratch.".to_vec();
            k.extend_from_slice(&n.to_be_bytes());
            s.put(k, vec![1]).unwrap();
        }
        // A key in the same leaf that diverges at byte 5: the prefix has to give
        // way, and every tail in the leaf is rewritten.
        s.put(b"meta!".to_vec(), vec![2]).unwrap();
        let all = drain(&s, b"", None);
        assert_eq!(all.len(), 65);
        assert_eq!(all[0].0, b"meta!");
        assert_eq!(all[1].0, {
            let mut k = b"meta.scratch.".to_vec();
            k.extend_from_slice(&0u64.to_be_bytes());
            k
        });
        for n in [0u64, 31, 63] {
            let mut k = b"meta.scratch.".to_vec();
            k.extend_from_slice(&n.to_be_bytes());
            assert_eq!(s.get(&k).unwrap(), Some(vec![1]), "key {n}");
        }
        assert_eq!(s.get(b"meta!").unwrap(), Some(vec![2]));
    }

    #[test]
    fn an_empty_value_is_an_entry_and_deleting_it_leaves_none() {
        let s = EphemeralStore::new();
        s.put(key(1), Vec::new()).unwrap();
        assert_eq!(s.get(&key(1)).unwrap(), Some(Vec::new()));
        s.del(&key(1)).unwrap();
        assert_eq!(s.get(&key(1)).unwrap(), None);
        // A delete of something absent is not a failure.
        s.del(&key(1)).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn overwrite_keeps_one_entry_and_the_latest_value() {
        let s = EphemeralStore::new();
        for round in 0..8u8 {
            s.put(key(7), vec![round; 40]).unwrap();
        }
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(&key(7)).unwrap(), Some(vec![7u8; 40]));
    }

    #[test]
    fn the_range_scan_is_half_open_and_in_byte_order() {
        let s = EphemeralStore::new();
        for n in (0..200).rev() {
            s.put(key(n), n.to_be_bytes().to_vec()).unwrap();
        }
        let got: Vec<Vec<u8>> = drain(&s, &key(50), Some(&key(60)))
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(got.len(), 10);
        assert_eq!(got[0], key(50));
        assert_eq!(got[9], key(59));
        let unbounded: Vec<Vec<u8>> = drain(&s, &key(195), None)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(unbounded.len(), 5);
    }

    #[test]
    #[allow(clippy::while_let_on_iterator)] // the point is to pull from ONE cursor
    fn both_ends_meet_without_overlap() {
        let s = EphemeralStore::new();
        for n in 0..101u64 {
            s.put(key(n), Vec::new()).unwrap();
        }
        let mut it = s.scan_range(b"", None);
        let mut front = Vec::new();
        for _ in 0..40 {
            front.push(it.next().unwrap().unwrap().0);
        }
        let mut back = Vec::new();
        for _ in 0..40 {
            back.push(it.next_back().unwrap().unwrap().0);
        }
        // Twenty-one left in the middle, and the two walks never overlapped.
        let mut rest = Vec::new();
        while let Some(item) = it.next() {
            rest.push(item.unwrap().0);
        }
        while let Some(item) = it.next_back() {
            rest.push(item.unwrap().0);
        }
        assert_eq!(front.len() + back.len() + rest.len(), 101);
        let mut all: Vec<Vec<u8>> = front;
        all.extend(rest);
        all.extend(back.into_iter().rev());
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 101);
    }

    #[test]
    fn seek_moves_forward_and_a_backwards_seek_is_a_no_op() {
        let s = EphemeralStore::new();
        for n in 0..50u64 {
            s.put(key(n), Vec::new()).unwrap();
        }
        let mut it = s.scan_range(b"", None);
        assert_eq!(it.next().unwrap().unwrap().0, key(0));
        assert!(it.seek(&key(20)));
        assert_eq!(it.next().unwrap().unwrap().0, key(20));
        // Behind the current position: keeps it.
        assert!(it.seek(&key(5)));
        assert_eq!(it.next().unwrap().unwrap().0, key(21));
        // Past the end: exhausted.
        assert!(!it.seek(b"zzz"));
        assert!(it.next().is_none());
    }

    #[test]
    fn a_prefix_scan_strips_the_prefix() {
        let s = EphemeralStore::new();
        s.put(b"aa1".to_vec(), b"x".to_vec()).unwrap();
        s.put(b"aa2".to_vec(), b"y".to_vec()).unwrap();
        s.put(b"ab1".to_vec(), b"z".to_vec()).unwrap();
        let mut suffixes = s.scan_suffix(b"aa").unwrap();
        suffixes.sort();
        assert_eq!(suffixes, vec![b"1".to_vec(), b"2".to_vec()]);
    }

    #[test]
    fn a_batch_is_one_lock_so_a_reader_never_sees_its_first_half() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let s = EphemeralStore::new();
        s.put(key(1), b"a".to_vec()).unwrap();
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let s = s.clone();
            let flag = std::sync::Arc::clone(&flag);
            std::thread::spawn(move || {
                let mut seen_first_half = false;
                while !flag.load(Ordering::Relaxed) {
                    if s.get(&key(1)).unwrap().as_deref() == Some(&b"1"[..]) {
                        seen_first_half = true;
                    }
                }
                seen_first_half
            })
        };
        for round in 0..5_000u32 {
            let mut batch = crate::engine::storage::Batch::default();
            batch.put(key(1), b"1".to_vec());
            batch.put(key(1), b"2".to_vec());
            s.commit_batch(batch).unwrap();
            // Reset for the next round, in its own batch.
            let mut reset = crate::engine::storage::Batch::default();
            reset.put(key(1), b"a".to_vec());
            s.commit_batch(reset).unwrap();
            assert_eq!(round, round);
        }
        flag.store(true, Ordering::Relaxed);
        assert!(
            !reader.join().unwrap(),
            "a reader saw the first put of a two-put batch: the batch was not one lock"
        );
    }

    #[test]
    fn a_shared_handle_is_the_same_keyspace() {
        let s = EphemeralStore::new();
        let h = s.shared_handle();
        h.put(key(9), b"v".to_vec()).unwrap();
        assert_eq!(s.get(&key(9)).unwrap().as_deref(), Some(&b"v"[..]));
        s.del(&key(9)).unwrap();
        assert!(h.get(&key(9)).unwrap().is_none());
    }

    #[test]
    fn a_scan_survives_writes_between_items() {
        let s = EphemeralStore::new();
        for n in 0..200u64 {
            s.put(key(n), Vec::new()).unwrap();
        }
        let it = s.scan_range(b"", None);
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for (n, item) in it.enumerate() {
            let n = n as u64;
            let (k, _) = item.unwrap();
            if seen.last().is_some() {
                assert!(seen.last().unwrap() < &k, "the cursor went backwards");
            }
            seen.push(k);
            // A write between items invalidates the cached cursor; the scan must
            // re-descend from where it was, not restart or skip.
            if n.is_multiple_of(7) {
                s.put(key(1000 + n), b"x".to_vec()).unwrap();
            }
            if n.is_multiple_of(11) {
                s.del(&key(n / 2)).unwrap();
            }
        }
        assert!(seen.len() >= 100, "the scan dropped items: {}", seen.len());
    }

    #[test]
    fn the_arena_is_rewritten_once_deletes_outweigh_the_live_half() {
        let s = EphemeralStore::new();
        let payload = vec![0x5Au8; 200];
        for n in 0..20_000u64 {
            s.put(key(n), payload.clone()).unwrap();
        }
        let full = s.bytes();
        for n in 0..18_000u64 {
            s.del(&key(n)).unwrap();
        }
        let after = s.bytes();
        assert!(
            after * 2 < full,
            "the arena did not shrink: {full} -> {after}"
        );
        assert_eq!(s.len(), 2_000);
        let survivors = drain(&s, b"", None);
        assert_eq!(survivors.len(), 2_000);
        assert_eq!(survivors[0].0, key(18_000));
        for (k, v) in &survivors {
            assert_eq!(s.get(k).unwrap().as_deref(), Some(v.as_slice()));
        }
    }

    #[test]
    fn a_random_operation_sequence_matches_a_btree_map() {
        let s = EphemeralStore::new();
        let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut rng = Rng(0xC0FF_EE12);
        for step in 0..20_000 {
            let n = rng.below(3_000);
            match rng.below(10) {
                0..=5 => {
                    let mut val = n.to_be_bytes().to_vec();
                    val.resize(rng.below(40) as usize, step as u8);
                    s.put(key(n), val.clone()).unwrap();
                    oracle.insert(key(n), val);
                }
                6..=7 => {
                    s.del(&key(n)).unwrap();
                    oracle.remove(&key(n));
                }
                _ => {
                    let want = oracle.get(&key(n)).cloned();
                    assert_eq!(s.get(&key(n)).unwrap(), want, "get {n} at step {step}");
                }
            }
            if step % 137 == 0 {
                assert_eq!(s.len(), oracle.len());
                let scanned: Vec<(Vec<u8>, Vec<u8>)> = drain(&s, b"", None);
                let expected: Vec<(Vec<u8>, Vec<u8>)> =
                    oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                assert_eq!(scanned, expected, "scan at step {step}");
            }
        }
        assert_eq!(s.len(), oracle.len());
        let scanned: Vec<(Vec<u8>, Vec<u8>)> = drain(&s, b"", None);
        let expected: Vec<(Vec<u8>, Vec<u8>)> =
            oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(scanned, expected, "final scan");
    }

    #[test]
    fn a_large_sequential_load_keeps_every_key_in_order() {
        let s = EphemeralStore::new();
        for n in 0..20_000u64 {
            s.put(key(n), n.to_be_bytes().to_vec()).unwrap();
        }
        assert_eq!(s.len(), 20_000);
        for n in [0u64, 1, 31, 32, 33, 10_000, 19_998, 19_999] {
            assert_eq!(
                s.get(&key(n)).unwrap(),
                Some(n.to_be_bytes().to_vec()),
                "key {n}"
            );
        }
        let all: Vec<Vec<u8>> = drain(&s, b"", None).into_iter().map(|(k, _)| k).collect();
        assert_eq!(all.len(), 20_000);
        for (i, k) in all.iter().enumerate() {
            assert_eq!(k, &key(i as u64), "position {i}");
        }
    }

    #[test]
    fn random_removal_thins_the_tree_without_losing_keys() {
        let s = EphemeralStore::new();
        for n in 0..20_000u64 {
            s.put(key(n), Vec::new()).unwrap();
        }
        let mut left: Vec<u64> = (0..20_000).collect();
        let mut rng = Rng(0x1234_5678);
        for _ in 0..15_000 {
            let i = rng.below(left.len() as u64) as usize;
            let n = left.swap_remove(i);
            s.del(&key(n)).unwrap();
        }
        left.sort_unstable();
        assert_eq!(s.len(), left.len());
        let all: Vec<Vec<u8>> = drain(&s, b"", None).into_iter().map(|(k, _)| k).collect();
        let want: Vec<Vec<u8>> = left.iter().map(|n| key(*n)).collect();
        assert_eq!(all, want);
        for n in &left {
            assert_eq!(s.get(&key(*n)).unwrap(), Some(Vec::new()), "key {n}");
        }
    }
}
