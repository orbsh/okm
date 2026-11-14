//! Graph Edge (ADR-0017, ADR-0067): the third relation carrier — for DYNAMIC
//! graphs (knowledge graphs). Bidirectional, attributes, and a declaration that
//! IS the graph.
//!
//! Division line vs the other two carriers (ADR-0015 §3, amended):
//! `Refs` = one-to-many (fixed child type, field position, one direction); a
//! relation table = many-to-many with compile-time typed endpoints; **Graph
//! Edge** = attributes, clusters, and a declaration that names its members.
//!
//! # An edge has no identity of its own (ADR-0067)
//!
//! An edge IS the tuple `(src, predicate, dst)`. There is no edge number: nothing
//! to allocate, nothing to preserve across a restore, nothing for a caller to
//! remember. Writing the same edge twice writes the same row, so idempotence is a
//! property of the key rather than a claim in the docs.
//!
//! # Endpoints come from the declared member tables
//!
//! A `NodeRef` is `[ns 2B][pkey]`. The ns names the collection and the pkey is
//! that collection's key encoding — fixed width by declaration (`KeyEncode`
//! emits one byte count per declaration). `#[ok_nodes(Table, …)]` is a closed
//! compile-time set, so the graph knows every member's key width, and an
//! endpoint whose ns is not a member is [`Error::ForeignEndpoint`] at write time.
//!
//! # Slot layout (fixed faces within the edge ns, ADR-0030/ADR-0017 §3)
//!
//! ```text
//! 0x01    dynamic segment   [src][pred][dst]               → the edge's open attributes (nTLV)
//! 0x02/0x03  field-name vocabulary (the attributes' own names ↔ ids)
//! 0x04/0x05  PREDICATE VOCABULARY      (predicate name ↔ predicate_vocab_id)
//! 0x06    out               [src][pred][dst]                → the edge payload
//! 0x07    in                [dst][pred][src]                → empty
//! 0x08    predicate+out     [pred][src][dst]                → empty
//! 0x09    predicate+in      [pred][dst][src]                → empty
//! ```
//!
//! A reference in a face is `[ns 2B][pkey]` — **no length**. The width is the
//! declaration's (see [`Members`]), so `[src][pred][dst]` parses by width alone:
//! read 2 bytes of ns, look its width up in the member table, take the pkey, skip
//! the 2 predicate bytes, repeat. Ordering is then value order, and a hop costs
//! zero extra bytes.
//!
//! **0x06 IS the edge.** It used to be an index that pointed at a row keyed by an
//! edge number; with the number gone, the traversal entry and the payload are the
//! same row. `0x07`/`0x08`/`0x09` are derived from it and carry nothing.
//!
//! **Every index carries BOTH endpoints and the predicate.** A face entry states
//! where an edge came from, what it is and where it went, so the tuple is
//! addressable: `(src, dst)` on 0x06, `(predicate, src, dst)` on 0x08. That is
//! what makes "is this fact already here" one seek instead of a scan over a hub's
//! whole out-degree — and it is why a traversal step knows the predicate.
//!
//! **There is no separate predicate index.** It used to be a slot of its own
//! (`[predicate][edge_id]`); scanning 0x08 by its `[predicate]` prefix yields
//! exactly the same edge set with longer keys.
//!
//! A vocabulary precedes the faces that read its ids. Declared attribute faces
//! are ordinary `#[ok_index]` declarations — counting family 0x3nnn, exactly the
//! document machinery (`Document::index_entries`), because the edge's payload IS
//! an obj (ADR-0012). The node collection is a STANDARD document collection: zero
//! layout difference from any document collection; a node collection carrying
//! kind tokens declares its own predicate vocabulary (the same 0x04/0x05 pair,
//! per collection).

use crate::engine::storage::{Batch, ScanIter, VirtualStorage, prefix_end, scan_suffix_kv};
use crate::model::index::{DYNAMIC_SLOT, Document, PRIMARY_SLOT, Slot, write_slot};
use crate::model::key::KeyEncode;
use crate::model::obj_vocab::{Faces, PREDICATE_VOCAB, Vocabulary};
use okm_value::{Dict, decode_named};

/// Fixed face 0x06: the edge itself — `[src][predicate][dst]` → payload.
///
/// It is the traversal face AND the row: a prefix on `src` is "all of a node's
/// out-edges", a prefix on `src + predicate` is the typed subset, and the whole
/// tuple is the assertion (ADR-0067).
pub const EDGE_OUT_SLOT: Slot = 0x06;
/// Fixed face 0x07: the in-edge traversal, `[dst][predicate][src]` → empty.
pub const EDGE_IN_SLOT: Slot = 0x07;
/// Fixed face 0x08: predicate-leading out edges — `[predicate][src][dst]`.
/// Its bare `[predicate]` prefix is the predicate index the graph used to keep
/// in its own slot.
pub const EDGE_PREDICATE_OUT_SLOT: Slot = 0x08;
/// Fixed face 0x09: predicate-leading in edges — `[predicate][dst][src]`.
pub const EDGE_PREDICATE_IN_SLOT: Slot = 0x09;

/// An endpoint reference: `[ns 2B BE][pkey]`.
///
/// The ns is the collection tag (one ns per collection, never reused) and the
/// pkey is that collection's own key encoding, whose width the declaration
/// fixes. The ref therefore carries **no length**: a key that holds one reads its
/// width from the member table ([`Members`]), which is what makes `[src][pred]
/// [dst]` parse by width, sort by value and cost nothing per hop (ADR-0067).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeRef {
    /// Big-endian `[ns 2B]` (the raw header bytes, same shape as
    /// `Document::NS_PREFIX`).
    pub ns: [u8; 2],
    /// Bare pkey bytes (no header, no length).
    pub pkey: Vec<u8>,
}

impl NodeRef {
    /// Assemble from a document's `NS_PREFIX` (or any raw `[ns 2B]`
    /// header) and its key encoding.
    pub fn new(ns_prefix: &[u8], pkey: &[u8]) -> Self {
        assert!(ns_prefix.len() == 2, "NodeRef ns header must be [ns 2B]");
        Self {
            ns: [ns_prefix[0], ns_prefix[1]],
            pkey: pkey.to_vec(),
        }
    }

    /// The `[ns 2B]` header slice.
    pub fn ns_prefix(&self) -> &[u8] {
        &self.ns
    }

    /// u16 view of the ns.
    pub fn ns_id(&self) -> u16 {
        u16::from_be_bytes(self.ns)
    }

    /// The key form: `[ns 2B][pkey]`, no length prefix (ADR-0067).
    pub fn wire(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + self.pkey.len());
        self.write(&mut buf);
        buf
    }

    /// Append [`wire`](Self::wire) to `buf`.
    pub fn write(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.ns);
        buf.extend_from_slice(&self.pkey);
    }

    /// Parse `[ns 2B][pkey @ width(ns)]` from the front of `bytes`; answers the
    /// ref and the bytes consumed, or `None` when the ns is unknown to `members`
    /// (a foreign endpoint is not this graph's row) or the buffer is short.
    pub fn parse(bytes: &[u8], members: &Members) -> Option<(Self, usize)> {
        if bytes.len() < 2 {
            return None;
        }
        let ns = u16::from_be_bytes([bytes[0], bytes[1]]);
        let width = members.key_len(ns)?;
        let total = 2 + width;
        if bytes.len() < total {
            return None;
        }
        Some((
            Self {
                ns: [bytes[0], bytes[1]],
                pkey: bytes[2..total].to_vec(),
            },
            total,
        ))
    }
}

/// The member tables' `(ns, key width)` pairs — the declaration's width table
/// (ADR-0067).
///
/// It is the whole reason an edge key needs no length prefix: a fixed-width
/// declaration turns "where does this endpoint end" into a lookup, and the lookup
/// is compile-time for a static graph ([`NodeSet::KEY_LENS`]) and
/// declaration-time for a dynamic one (the exported schema).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Members(Vec<(u16, usize)>);

impl Members {
    /// From `(ns, key width)` pairs in declaration order.
    pub fn new(members: Vec<(u16, usize)>) -> Self {
        Self(members)
    }

    /// The width table of a generated member set — the static graph's form.
    pub fn of<N: NodeSet>() -> Self {
        Self(
            N::NS_VALUES
                .iter()
                .copied()
                .zip(N::KEY_LENS.iter().copied())
                .collect(),
        )
    }

    /// The member's pkey width, or `None` when the ns is not a member.
    pub fn key_len(&self, ns: u16) -> Option<usize> {
        self.0
            .iter()
            .find(|(member, _)| *member == ns)
            .map(|(_, width)| *width)
    }

    /// Is `ns` a member of this graph?
    pub fn contains(&self, ns: u16) -> bool {
        self.key_len(ns).is_some()
    }

    /// Every member ns, declaration order.
    pub fn ns_values(&self) -> impl Iterator<Item = u16> + '_ {
        self.0.iter().map(|(ns, _)| *ns)
    }

    /// Does this table declare nothing? (A writer without a graph declaration.)
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The pairs themselves, declaration order.
    pub fn pairs(&self) -> &[(u16, usize)] {
        &self.0
    }
}

/// An edge's identity as a key: `[src_ns][src_pkey][predicate u16][dst_ns]
/// [dst_pkey]` — the tuple, and nothing else (ADR-0067).
///
/// The width of each endpoint is the declaration's, so `KEY_LEN` — the
/// fixed-width contract [`KeyEncode`] states — has no single value here and is
/// `0`: no document collection is keyed by an edge, and the only reader of an
/// edge key is the graph, which parses by [`Members`]. `encode` is the truth.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EdgeKey {
    pub src: NodeRef,
    /// The predicate's vocabulary id (0x04/0x05) — a declaration-time value
    /// once the write has interned the name.
    pub predicate: u16,
    pub dst: NodeRef,
}

impl EdgeKey {
    pub fn new(src: NodeRef, predicate: u16, dst: NodeRef) -> Self {
        Self {
            src,
            predicate,
            dst,
        }
    }

    /// Append the identity's bytes: no lengths, no id.
    pub fn write(&self, buf: &mut Vec<u8>) {
        self.src.write(buf);
        buf.extend_from_slice(&self.predicate.to_be_bytes());
        self.dst.write(buf);
    }

    /// The identity's bytes.
    pub fn wire(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 + self.src.pkey.len() + self.dst.pkey.len());
        self.write(&mut buf);
        buf
    }

    /// Parse `[src][predicate][dst]` from the front of `bytes` by width, and
    /// answer the key and the bytes consumed.
    ///
    /// Width is the whole parse: two bytes of ns name the member table, whose
    /// declaration says how many pkey bytes follow (ADR-0067).
    pub fn parse(bytes: &[u8], members: &Members) -> Option<(Self, usize)> {
        let (src, n1) = NodeRef::parse(bytes, members)?;
        let rest = &bytes[n1..];
        if rest.len() < 2 {
            return None;
        }
        let predicate = u16::from_be_bytes([rest[0], rest[1]]);
        let (dst, n2) = NodeRef::parse(&rest[2..], members)?;
        Some((
            Self {
                src,
                predicate,
                dst,
            },
            n1 + 2 + n2,
        ))
    }
}

impl KeyEncode for EdgeKey {
    /// No single number: the identity's width is the graph declaration's
    /// (`Members`), because two endpoints may be different member tables. Nothing
    /// slices an edge key by this value — a graph payload's declared faces read
    /// the key through [`encode`](Self::encode).
    const KEY_LEN: usize = 0;
    const FIELD_WIDTHS: &'static [(&'static str, usize)] = &[];

    fn encode(&self) -> Vec<u8> {
        self.wire()
    }

    /// Unreachable by construction: an edge key is decoded through the graph's
    /// width table ([`EdgeKey::parse`]), never by a fixed width.
    fn decode(_bytes: &[u8]) -> Self {
        panic!("an edge key has no fixed width — parse it through the graph's member table")
    }

    fn encode_prefix_named(&self, buf: &mut Vec<u8>, names: &[&str]) -> usize {
        assert!(
            names.is_empty(),
            "EdgeKey declares no nameable fields; a prefix must be empty"
        );
        let _ = buf;
        0
    }

    fn prefix_width(names: &[&str]) -> usize {
        assert!(
            names.is_empty(),
            "EdgeKey declares no nameable fields; a prefix must be empty"
        );
        0
    }
}

/// The member set of a graph: one variant per participating COLLECTION
/// (each variant is one ns), and the pkey width that collection's declaration
/// fixes. A declaration, not storage — the set carries no run-time information,
/// so it never occupies a slot. Generated by `#[derive(GraphEncode)]` from
/// `#[ok_nodes(Table, …)]`, never written by hand: the ns of a variant is read
/// off the table's own `#[ok_ns]` and the width off its key's `KEY_LEN`.
pub trait NodeSet: Copy {
    /// Every member collection's ns, one per variant.
    const NS_VALUES: &'static [u16];

    /// Every member collection's key width, aligned with [`Self::NS_VALUES`].
    ///
    /// This one table is what lets an edge key carry no length prefix: the
    /// declaration already says how wide each endpoint is (ADR-0067).
    const KEY_LENS: &'static [usize];

    /// The variant's POSITION — its index into [`Self::NS_VALUES`], which the
    /// derive emits in declaration order. It exists because a write path is
    /// handed a variant and has to name its collection (which ns to write
    /// under), and the enum is fieldless: the discriminant IS the position.
    fn index(self) -> usize;

    /// The member collection's ns.
    fn ns(self) -> u16 {
        Self::NS_VALUES[self.index()]
    }

    /// Is `ns` a member of this graph?
    fn contains(ns: u16) -> bool {
        Self::NS_VALUES.contains(&ns)
    }

    /// The member table's key width — `None` for a namespace this graph does not
    /// admit.
    fn key_len(ns: u16) -> Option<usize> {
        Self::NS_VALUES
            .iter()
            .position(|member| *member == ns)
            .and_then(|at| Self::KEY_LENS.get(at).copied())
    }
}

/// The graph declaration (ADR-0017 §4), generated by `#[derive(GraphEncode)]`:
/// `#[ok_ns(N)]` is the only ns source, the struct's fields are the edge's
/// payload (an ordinary document payload — both codec directions, its
/// `#[ok_index]` faces declared EXPLICITLY), and `#[ok_nodes(Table, …)]` lists
/// the member collections by their document type. `Graph<S, E>` is a handle
/// over this and declares nothing.
pub trait GraphEncode: Document<Key = EdgeKey> {
    /// The member set — generated from `#[ok_nodes]`: an enum over the listed
    /// node collections, its ns values and key widths taken from the tables
    /// themselves.
    type Nodes: NodeSet;
}

/// One edge's head: endpoints by reference, predicate by name. The payload
/// travels separately (the declaration's own type) and is encoded by the
/// graph at write time.
///
/// The head IS the identity: resolve the predicate once
/// ([`Graph::key_of`]) and the tuple is what every verb takes. A `NodeRef`
/// built by hand is checked at write time ([`Error::ForeignEndpoint`]); the
/// generated member enum (`EmploymentNode::user(&key)`) is the checked way to
/// build one.
#[derive(Clone, Debug)]
pub struct EdgeHead {
    pub src: NodeRef,
    pub dst: NodeRef,
    /// The edge's type: resolved to `predicate_vocab_id` through the graph's kind
    /// vocabulary (0x04/0x05) at write time, so every key segment stays
    /// fixed-width. Never repeated in the payload.
    pub predicate: String,
}

/// One edge read back: its identity (from the key it was read at) and its
/// payload bytes.
#[derive(Clone, Debug)]
pub struct EdgeBody {
    pub src: NodeRef,
    pub predicate_vocab_id: u16,
    pub dst: NodeRef,
    pub payload: Vec<u8>,
}

/// Engine + graph type = the operation surface of one graph's edge set
/// (symmetric with a relation collection (ADR-0051)). One ns hosts every entry kind,
/// dispatched by segment. The `Vocabulary` is per-handle state over the
/// graph's PREDICATE VOCABULARY (0x04/0x05) — allocation is single-writer, and a
/// read-only handle still answers typed scans (the cache loads on probe).
pub struct Graph<S, E: GraphEncode> {
    store: S,
    vocab: Vocabulary<u16>,
    /// The ns the edge tables live under. `E::NS_PREFIX` unless a caller moved
    /// the whole graph — the restore's mapping (ADR-0062): an edge's endpoints
    /// carry the ns in their refs, so the graph moves as one.
    ns: [u8; 2],
    /// The member tables' widths, from the declaration — the table every key
    /// parse reads.
    members: Members,
    _pd: std::marker::PhantomData<E>,
}

impl<S: VirtualStorage, E: GraphEncode> Graph<S, E> {
    /// The handle takes the engine and nothing else: the ns comes from the
    /// declaration (`E::NS_PREFIX`), the member set from `E::Nodes`.
    pub fn new(store: S) -> Self {
        Self::with_ns(store, E::NS)
    }

    /// The same graph rooted at ANOTHER ns — the restore's mapping (ADR-0062).
    ///
    /// One ns for the whole graph, not one per table: an edge's endpoints are
    /// ns refs, so an edge restored next to nodes that stayed behind would
    /// point at nothing. What moves is the graph, and the node table moves with it
    /// because the caller builds that collection under the same ns.
    pub fn with_ns(store: S, ns: u16) -> Self {
        Self {
            store,
            vocab: Vocabulary::new(),
            ns: ns.to_be_bytes(),
            members: Members::of::<E::Nodes>(),
            _pd: std::marker::PhantomData,
        }
    }

    /// Whether the edge table holds no edge at all — the restore's emptiness
    /// check for a graph (ADR-0062): nodes and edges share one ns, so "the
    /// namespace is empty" has to ask about both.
    pub fn has_edges(&self) -> crate::error::Result<bool> {
        let prefix = self.face_key(EDGE_OUT_SLOT, &[]);
        let end = crate::engine::storage::prefix_end(&prefix);
        Ok(self
            .store
            .scan_range(&prefix, end.as_deref())
            .next()
            .is_some())
    }

    /// The same graph rooted at another ns, over the same engine (ADR-0062).
    pub fn rebased(&self, ns: u16) -> Self
    where
        S: Clone,
    {
        Self {
            store: self.store.clone(),
            vocab: Vocabulary::new(),
            ns: ns.to_be_bytes(),
            members: self.members.clone(),
            _pd: std::marker::PhantomData,
        }
    }

    /// The ns this graph is rooted at.
    pub fn ns(&self) -> u16 {
        u16::from_be_bytes(self.ns)
    }

    /// The declaration's width table (ns → key width).
    pub fn member_widths(&self) -> &Members {
        &self.members
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Mutable access to the underlying storage — the restore path's need:
    /// assembling a row's bytes interns vocabulary labels, which is a write,
    /// and the write happens before the row's own batch is committed.
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    fn face_key(&self, slot: Slot, segments: &[&[u8]]) -> Vec<u8> {
        let mut k = self.ns.to_vec();
        write_slot(&mut k, slot);
        for s in segments {
            k.extend_from_slice(s);
        }
        k
    }

    /// `[ns][0x06][src][pred][dst]` — the edge itself.
    fn out_key(&self, key: &EdgeKey) -> Vec<u8> {
        let mut k = self.face_key(EDGE_OUT_SLOT, &[]);
        key.write(&mut k);
        k
    }

    /// `[ns][0x07][dst][pred][src]`.
    fn in_key(&self, key: &EdgeKey) -> Vec<u8> {
        let mut k = self.face_key(EDGE_IN_SLOT, &[]);
        key.dst.write(&mut k);
        k.extend_from_slice(&key.predicate.to_be_bytes());
        key.src.write(&mut k);
        k
    }

    /// `[ns][0x08][pred][src][dst]`.
    fn predicate_out_key(&self, key: &EdgeKey) -> Vec<u8> {
        let mut k = self.face_key(EDGE_PREDICATE_OUT_SLOT, &[&key.predicate.to_be_bytes()]);
        key.src.write(&mut k);
        key.dst.write(&mut k);
        k
    }

    /// `[ns][0x09][pred][dst][src]`.
    fn predicate_in_key(&self, key: &EdgeKey) -> Vec<u8> {
        let mut k = self.face_key(EDGE_PREDICATE_IN_SLOT, &[&key.predicate.to_be_bytes()]);
        key.dst.write(&mut k);
        key.src.write(&mut k);
        k
    }

    /// The edge's DYNAMIC segment (face 0x01) — its open attributes, one entry
    /// per edge, keyed `[ns][0x01][src][pred][dst]`.
    fn dynamic_key(&self, key: &EdgeKey) -> Vec<u8> {
        let mut k = self.face_key(DYNAMIC_SLOT, &[]);
        key.write(&mut k);
        k
    }

    /// Endpoint membership and width, checked at write: the ns must be one of the
    /// declared member tables, and the pkey must be exactly as wide as that
    /// table's declaration — otherwise the key would not parse by width, and a
    /// foreign endpoint would be written as if it belonged here
    /// ([`Error::ForeignEndpoint`]).
    fn check_endpoint(&self, side: &'static str, node: &NodeRef) -> crate::error::Result<()> {
        let ns = node.ns_id();
        let Some(width) = self.members.key_len(ns) else {
            return Err(crate::error::Error::ForeignEndpoint {
                side,
                ns,
                graph_ns: self.ns(),
            });
        };
        if node.pkey.len() != width {
            return Err(crate::error::Error::Corrupt {
                what: format!(
                    "the {side} endpoint in ns {ns} is {} byte(s) wide, but that table's key is {width} — \
                     a graph member must have a fixed-width key (ADR-0067)",
                    node.pkey.len()
                ),
            });
        }
        Ok(())
    }

    /// Resolve (or allocate) the predicate id through the graph's kind
    /// vocabulary (0x04/0x05) — first-seen-claims-next, append-only, the
    /// same discipline as every other vocabulary in OKM. Separate from the
    /// payload's field-name vocabulary (0x02/0x03): two allocators in one
    /// id space would hand the same id to two names.
    fn predicate_vocab_id(&mut self, predicate: &str) -> crate::error::Result<u16> {
        let Self { store, vocab, .. } = self;
        let mut door = Faces::<_, u16>::new(&*store, &self.ns, PREDICATE_VOCAB);
        vocab.ensure(&mut door, predicate)
    }

    /// The predicate's id if it was ever interned — no allocation. `None` means
    /// nobody wrote this predicate, so no edge can carry it.
    fn vocab_lookup(&mut self, predicate: &str) -> crate::error::Result<Option<u16>> {
        let Self { store, vocab, .. } = self;
        let mut door = Faces::<_, u16>::new(&*store, &self.ns, PREDICATE_VOCAB);
        vocab.id_of(&mut door, predicate)
    }

    /// Resolve predicate id → name for display/diagnostics.
    pub fn predicate_vocab_name(&mut self, id: u16) -> crate::error::Result<Option<String>> {
        let Self { store, vocab, .. } = self;
        let mut door = Faces::<_, u16>::new(&*store, &self.ns, PREDICATE_VOCAB);
        vocab.name_of(&mut door, id)
    }

    /// Every predicate this graph has, id-ordered: ONE prefix scan of 0x04.
    /// A listing, never an inference walked back from the index faces.
    pub fn predicates(&mut self) -> crate::error::Result<Vec<(u16, String)>> {
        let mut p = self.ns.to_vec();
        write_slot(&mut p, crate::model::index::PREDICATE_VOCAB_ID_SLOT);
        let mut out: Vec<(u16, String)> = scan_suffix_kv(&self.store, &p)?
            .into_iter()
            .filter(|(suffix, _)| suffix.len() == 2)
            .filter_map(|(suffix, name)| {
                String::from_utf8(name)
                    .ok()
                    .map(|n| (u16::from_be_bytes([suffix[0], suffix[1]]), n))
            })
            .collect();
        out.sort_by_key(|(id, _)| *id);
        Ok(out)
    }

    /// The declaration's identity for a head: the predicate's vocabulary id, or
    /// `None` when no edge ever carried that predicate (so no row can hold it).
    ///
    /// This is the one place a name becomes an identity, and it does not allocate
    /// on a read path — a head naming an unwritten predicate simply has no key.
    pub fn key_of(&mut self, head: &EdgeHead) -> crate::error::Result<Option<EdgeKey>> {
        self.check_endpoint("src", &head.src)?;
        self.check_endpoint("dst", &head.dst)?;
        Ok(self
            .vocab_lookup(&head.predicate)?
            .map(|predicate| EdgeKey {
                src: head.src.clone(),
                predicate,
                dst: head.dst.clone(),
            }))
    }

    /// The identity a WRITE will use: endpoints checked against the declaration
    /// and the predicate interned (or allocated), because the vocabulary id is
    /// part of the key and the existence probe has to happen after it.
    ///
    /// Two heads that name the same predicate answer the same key, which is what
    /// makes a second write of an edge a re-assertion rather than a second row.
    pub(crate) fn write_key(&mut self, head: &EdgeHead) -> crate::error::Result<EdgeKey> {
        self.check_endpoint("src", &head.src)?;
        self.check_endpoint("dst", &head.dst)?;
        let predicate = self.predicate_vocab_id(&head.predicate)?;
        Ok(EdgeKey {
            src: head.src.clone(),
            predicate,
            dst: head.dst.clone(),
        })
    }

    /// Read one edge's payload bytes at its identity — one point read, no
    /// decode in between: the key IS the row.
    pub fn get_edge(&self, key: &EdgeKey) -> crate::error::Result<Option<EdgeBody>> {
        // A key naming a foreign endpoint cannot be this graph's row, and the
        // width lookup below would not know how to build it either.
        self.check_endpoint("src", &key.src)?;
        self.check_endpoint("dst", &key.dst)?;
        let Some(payload) = self.store.get(&self.out_key(key))? else {
            return Ok(None);
        };
        Ok(Some(EdgeBody {
            src: key.src.clone(),
            predicate_vocab_id: key.predicate,
            dst: key.dst.clone(),
            payload,
        }))
    }

    /// The edge's payload decoded into the declaration's own type.
    pub fn edge(&self, key: &EdgeKey) -> crate::error::Result<Option<E>> {
        Ok(self
            .get_edge(key)?
            .map(|body| E::decode_payload(&body.payload)))
    }

    /// The edge's open attributes, when it has any — the dynamic half of an edge
    /// (its typed half is the payload, which is not a map). A name whose id the
    /// vocabulary cannot resolve is dropped rather than invented, and an empty
    /// segment answers `None` (no attributes) — the same rule the document path
    /// follows.
    pub fn attrs(&self, key: &EdgeKey) -> crate::error::Result<Option<Dict>> {
        let raw = match self.store.get(&self.dynamic_key(key))? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        let mut dict = Vocabulary::<u16>::new();
        let mut door =
            Faces::<_, u16>::new(&self.store, &self.ns, crate::model::obj_vocab::FIELD_VOCAB);
        let mut failed: Option<crate::error::Error> = None;
        let frames = decode_named(&raw, &mut |id| match dict.name_of(&mut door, id) {
            Ok(name) => name,
            Err(e) => {
                failed = Some(e);
                None
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        if frames.is_empty() {
            return Ok(None);
        }
        let mut out = Dict::new();
        for frame in frames {
            if let Some(name) = dict.name_of(&mut door, frame.id)? {
                out.insert(name, frame.value);
            }
        }
        Ok(Some(out))
    }

    /// Every edge in this graph, in key order — the scan an export needs.
    pub fn edges(&self) -> crate::error::Result<Vec<EdgeKey>> {
        self.edge_keys_from(self.face_key(EDGE_OUT_SLOT, &[]))
    }

    /// Parse every entry of a 0x06 prefix into an identity. A row this
    /// declaration cannot read (a foreign ns, a truncated tuple) is skipped
    /// rather than guessed at — the same fail-closed direction every face read
    /// takes.
    fn edge_keys_from(&self, prefix: Vec<u8>) -> crate::error::Result<Vec<EdgeKey>> {
        let mut out = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &prefix)? {
            let Some((key, taken)) = EdgeKey::parse(&suffix, &self.members) else {
                continue;
            };
            if taken == suffix.len() {
                out.push(key);
            }
        }
        Ok(out)
    }

    /// Replace the edge's open attributes, in the SAME batch as the row so one
    /// commit carries both. Names allocate in the edge ns's field-name
    /// vocabulary (0x02/0x03), which is separate from the predicate vocabulary
    /// the row's write touches.
    ///
    /// A rewrite replaces the segment outright: the previous assertion's
    /// attributes are deleted first, so nothing inherits a stale prop — this is
    /// `replace` semantics, not a per-name merge (an edge has no partial-update
    /// caller yet; add `merge_into` the day one appears).
    pub fn replace_into(
        &mut self,
        batch: &mut Batch,
        key: &EdgeKey,
        fields: &Dict,
    ) -> crate::error::Result<()> {
        let key = self.dynamic_key(key);
        batch.del(&key);
        if fields.is_empty() {
            return Ok(());
        }
        let body = {
            let mut dict = Vocabulary::<u16>::new();
            let mut door =
                Faces::<_, u16>::new(&self.store, &self.ns, crate::model::obj_vocab::FIELD_VOCAB);
            let mut resolver = |name: &str| dict.ensure(&mut door, name);
            okm_value::encode(fields, &mut resolver)?
        };
        batch.put(key, body);
        Ok(())
    }

    /// Encode one edge's full row set into the write fragment's batch: the edge
    /// (0x06, payload included) + the three derived faces + every DECLARED
    /// `#[ok_index]` face, and the vocabulary pair on first sight of a new
    /// predicate. `pub(crate)` because [`Graph::incorporate`] is the only way to
    /// write an edge — this is the row encoder underneath it, never a verb of
    /// its own.
    pub(crate) fn link_into(
        &mut self,
        batch: &mut Batch,
        head: &EdgeHead,
        payload: &E,
    ) -> crate::error::Result<EdgeKey> {
        let key = self.write_rows(batch, head, payload)?;
        batch.del(&self.dynamic_key(&key));
        Ok(key)
    }

    /// Re-assert an edge that is already there: every row of the assertion,
    /// WITHOUT the dynamic segment's delete — the open attributes (0x01) belong
    /// to the stored assertion and are no declaration's to clear.
    pub(crate) fn assert_into(
        &mut self,
        batch: &mut Batch,
        head: &EdgeHead,
        payload: &E,
    ) -> crate::error::Result<EdgeKey> {
        self.write_rows(batch, head, payload)
    }

    /// The rows of one assertion, without touching the dynamic segment.
    fn write_rows(
        &mut self,
        batch: &mut Batch,
        head: &EdgeHead,
        payload: &E,
    ) -> crate::error::Result<EdgeKey> {
        self.check_endpoint("src", &head.src)?;
        self.check_endpoint("dst", &head.dst)?;
        let predicate = self.predicate_vocab_id(&head.predicate)?;
        let key = EdgeKey {
            src: head.src.clone(),
            predicate,
            dst: head.dst.clone(),
        };

        // 0x06 IS the edge: its value is the payload, so the traversal row and
        // the payload row are one write (ADR-0067).
        batch.put(self.out_key(&key), payload.encode_payload());
        // The three derived faces carry nothing: the key states everything.
        batch.put(self.in_key(&key), Vec::new());
        batch.put(self.predicate_out_key(&key), Vec::new());
        batch.put(self.predicate_in_key(&key), Vec::new());
        // Declared faces: the payload's own #[ok_index] declarations, the
        // same entries a document collection writes for the same row.
        for (entry_key, value) in E::index_entries(&key, payload, &self.ns) {
            batch.put(entry_key, value);
        }
        Ok(key)
    }

    /// Delete exactly the rows one edge wrote — the row, the three derived
    /// faces and the declared faces — and leave the open attributes (0x01) alone.
    ///
    /// The identity is the key, so no read is needed to name the faces; the
    /// payload is read only to re-derive the declared faces from it, so an
    /// unrelated edge with a shared attribute value cannot be touched.
    ///
    /// The dynamic segment is a segment of its own: a re-assertion of the same
    /// tuple must keep the attributes the stored assertion carries, which is why
    /// this verb does not clear them. Deleting the edge outright is
    /// [`purge_into`](Self::purge_into).
    pub(crate) fn unlink_into(
        &mut self,
        batch: &mut Batch,
        key: &EdgeKey,
    ) -> crate::error::Result<()> {
        let out = self.out_key(key);
        let payload = self
            .store
            .get(&out)?
            .ok_or(crate::error::Error::EdgeNotFound {
                src_ns: key.src.ns_id(),
                predicate: key.predicate,
                dst_ns: key.dst.ns_id(),
            })?;
        let payload: E = E::decode_payload(&payload);

        batch.del(&out);
        batch.del(&self.in_key(key));
        batch.del(&self.predicate_out_key(key));
        batch.del(&self.predicate_in_key(key));
        for (entry_key, _) in E::index_entries(key, &payload, &self.ns) {
            batch.del(&entry_key);
        }
        Ok(())
    }

    /// Remove the whole assertion: every row [`unlink_into`](Self::unlink_into)
    /// takes PLUS the open attributes. This is the delete verb's row encoder —
    /// an edge's attributes cannot outlive the edge they describe, because both
    /// are keyed by the same tuple.
    ///
    /// `pub(crate)` for the same reason the other encoders are: [`Graph::retract`]
    /// owns the verb.
    pub(crate) fn purge_into(
        &mut self,
        batch: &mut Batch,
        key: &EdgeKey,
    ) -> crate::error::Result<()> {
        batch.del(&self.dynamic_key(key));
        self.unlink_into(batch, key)
    }

    /// Out-edges of one node (0x06). A 0x06 entry's suffix is the rest of the
    /// tuple: the predicate and the far endpoint.
    pub fn out_edges(&self, src: &NodeRef) -> crate::error::Result<Vec<EdgeKey>> {
        let prefix = self.face_key(EDGE_OUT_SLOT, &[]);
        let mut full = prefix.clone();
        src.write(&mut full);
        let mut out = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &full)? {
            if suffix.len() < 2 {
                continue;
            }
            let predicate = u16::from_be_bytes([suffix[0], suffix[1]]);
            let Some((dst, taken)) = NodeRef::parse(&suffix[2..], &self.members) else {
                continue;
            };
            if taken + 2 != suffix.len() {
                continue;
            }
            out.push(EdgeKey {
                src: src.clone(),
                predicate,
                dst,
            });
        }
        Ok(out)
    }

    /// In-edges of one node (0x07). The face states `[predicate][src]`.
    pub fn in_edges(&self, dst: &NodeRef) -> crate::error::Result<Vec<EdgeKey>> {
        let mut prefix = self.face_key(EDGE_IN_SLOT, &[]);
        dst.write(&mut prefix);
        let mut out = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &prefix)? {
            if suffix.len() < 2 {
                continue;
            }
            let predicate = u16::from_be_bytes([suffix[0], suffix[1]]);
            let Some((src, taken)) = NodeRef::parse(&suffix[2..], &self.members) else {
                continue;
            };
            if taken + 2 != suffix.len() {
                continue;
            }
            out.push(EdgeKey {
                src,
                predicate,
                dst: dst.clone(),
            });
        }
        Ok(out)
    }

    /// Predicate-qualified traversal (0x08/0x09): all edge ids of one
    /// predicate from one node. An unknown predicate scans nothing — there
    /// is no face to match.
    fn typed_edges(
        &mut self,
        slot: Slot,
        predicate: &str,
        node: &NodeRef,
    ) -> crate::error::Result<Vec<EdgeKey>> {
        let Some(predicate_id) = self.vocab_lookup(predicate)? else {
            return Ok(Vec::new());
        };
        let mut prefix = self.face_key(slot, &[&predicate_id.to_be_bytes()]);
        node.write(&mut prefix);
        let mut out = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &prefix)? {
            let Some((far, taken)) = NodeRef::parse(&suffix, &self.members) else {
                continue;
            };
            if taken != suffix.len() {
                continue;
            }
            out.push(match slot {
                EDGE_PREDICATE_OUT_SLOT => EdgeKey {
                    src: node.clone(),
                    predicate: predicate_id,
                    dst: far,
                },
                _ => EdgeKey {
                    src: far,
                    predicate: predicate_id,
                    dst: node.clone(),
                },
            });
        }
        Ok(out)
    }

    /// Predicate-qualified out-edges (0x08).
    pub fn typed_out(
        &mut self,
        predicate: &str,
        src: &NodeRef,
    ) -> crate::error::Result<Vec<EdgeKey>> {
        self.typed_edges(EDGE_PREDICATE_OUT_SLOT, predicate, src)
    }

    /// Predicate-qualified in-edges (0x09).
    pub fn typed_in(
        &mut self,
        predicate: &str,
        dst: &NodeRef,
    ) -> crate::error::Result<Vec<EdgeKey>> {
        self.typed_edges(EDGE_PREDICATE_IN_SLOT, predicate, dst)
    }

    /// Edges from one node to another, whatever the predicate (0x06 prefix on
    /// both endpoints) — the pair is a prefix of the tuple, so this is one scan.
    /// The predicate sits between the endpoints, so `(src, dst)` is not a
    /// contiguous prefix of the tuple: this filters the source's run rather than
    /// opening a second face (ADR-0067 has no `(src, dst)` index, and does not
    /// need one — the run is the node's out-degree).
    pub fn out_edges_to(&self, src: &NodeRef, dst: &NodeRef) -> crate::error::Result<Vec<EdgeKey>> {
        Ok(self
            .out_edges(src)?
            .into_iter()
            .filter(|key| key.dst == *dst)
            .collect())
    }

    /// Edges into one node from another, whatever the predicate (0x07).
    pub fn in_edges_from(
        &self,
        dst: &NodeRef,
        src: &NodeRef,
    ) -> crate::error::Result<Vec<EdgeKey>> {
        Ok(self
            .in_edges(dst)?
            .into_iter()
            .filter(|key| key.src == *src)
            .collect())
    }

    /// The assertion identity lookup: one exact key, whatever the source node's
    /// out-degree. `None` when no edge carries the tuple.
    pub fn edge_at(
        &mut self,
        predicate: &str,
        src: &NodeRef,
        dst: &NodeRef,
    ) -> crate::error::Result<Option<EdgeKey>> {
        let head = EdgeHead {
            src: src.clone(),
            dst: dst.clone(),
            predicate: predicate.to_string(),
        };
        let Some(key) = self.key_of(&head)? else {
            return Ok(None);
        };
        Ok(self.get_edge(&key)?.map(|_| key))
    }

    /// All edges of one predicate, regardless of endpoints — the
    /// `MATCH ()-[r:has]->()` entry point. The bare `[predicate]` prefix of
    /// 0x08 IS the predicate index: this face used to be a slot of its own.
    pub fn edges_of_predicate(&mut self, predicate: &str) -> crate::error::Result<Vec<EdgeKey>> {
        let Some(predicate_id) = self.vocab_lookup(predicate)? else {
            return Ok(Vec::new());
        };
        let prefix = self.face_key(EDGE_PREDICATE_OUT_SLOT, &[&predicate_id.to_be_bytes()]);
        let mut out = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &prefix)? {
            let Some((src, n1)) = NodeRef::parse(&suffix, &self.members) else {
                continue;
            };
            let Some((dst, n2)) = NodeRef::parse(&suffix[n1..], &self.members) else {
                continue;
            };
            if n1 + n2 != suffix.len() {
                continue;
            }
            out.push(EdgeKey {
                src,
                predicate: predicate_id,
                dst,
            });
        }
        Ok(out)
    }

    /// Rebuild every DERIVED graph face (`0x07`/`0x08`/`0x09`) from the edges
    /// themselves, and sweep the retired primary slot.
    ///
    /// `0x06` is the authority (ADR-0067): it holds the tuple and the payload, so
    /// the three indexes are pure derivations of it. A store written before this
    /// layout also has `[ns][0x00][edge_id]` rows — the numbered edges — which
    /// this build never reads; they are swept, and nothing outside the edge
    /// namespace is touched.
    ///
    /// `chunk` bounds the ops per commit. A reader running DURING the rebuild
    /// sees a half-built face, which is why the operator contract is a window: do
    /// not read while it runs. `usize::MAX` restores the old single-commit shape.
    pub fn reindex(&mut self, chunk: usize) -> crate::error::Result<usize> {
        let chunk = chunk.max(1);
        let retired = [PRIMARY_SLOT];

        // Phase 1 — sweep the derived faces and the retired primary. A face entry
        // encodes the tuple it was derived from, so a moved edge answers at a key
        // this build never builds; re-deriving cannot name the old one.
        let mut batch = Batch::default();
        for slot in [
            EDGE_IN_SLOT,
            EDGE_PREDICATE_OUT_SLOT,
            EDGE_PREDICATE_IN_SLOT,
        ]
        .into_iter()
        .chain(retired)
        {
            let prefix = self.face_key(slot, &[]);
            let end = prefix_end(&prefix);
            for item in self.store.scan_range(&prefix, end.as_deref()) {
                let (key, _) = item?;
                batch.del(&key);
                if batch.ops.len() >= chunk {
                    self.store.commit_batch(std::mem::take(&mut batch))?;
                }
            }
        }
        if !batch.ops.is_empty() {
            self.store.commit_batch(std::mem::take(&mut batch))?;
        }

        // Phase 2 — derive from 0x06, lazily: the keys come from the walk, not
        // from a face just emptied.
        let mut rebuilt = 0usize;
        for key in self.edges()? {
            batch.put(self.in_key(&key), Vec::new());
            batch.put(self.predicate_out_key(&key), Vec::new());
            batch.put(self.predicate_in_key(&key), Vec::new());
            rebuilt += 1;
            if batch.ops.len() >= chunk {
                self.store.commit_batch(std::mem::take(&mut batch))?;
            }
        }
        if !batch.ops.is_empty() {
            self.store.commit_batch(batch)?;
        }
        Ok(rebuilt)
    }

    /// Filter by declared attribute (a counting-family 0x3nnn face): scan the
    /// face with the caller-encoded probe and return the identities behind it.
    /// The declared face's tail IS the edge key (same bytes, same widths).
    pub fn by_attr_face(&self, slot: Slot, encoded: &[u8]) -> crate::error::Result<Vec<EdgeKey>> {
        let p = self.face_key(slot, &[encoded]);
        let mut out = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &p)? {
            let Some((key, taken)) = EdgeKey::parse(&suffix, &self.members) else {
                continue;
            };
            if taken == suffix.len() {
                out.push(key);
            }
        }
        Ok(out)
    }

    /// The NEIGHBOR nodes one hop out (0x06): the entries resolved to their far
    /// endpoints. The identity is the tuple, so this is a composition over
    /// `out_edges` and nothing is re-read. Parallel edges collapse into repeated
    /// refs; dedupe if the caller wants a set (`NodeRef` is Hash + Ord for
    /// exactly this), or keep the repeats as multiplicity.
    pub fn out_nodes(&self, src: &NodeRef) -> crate::error::Result<Vec<NodeRef>> {
        Ok(self.out_edges(src)?.into_iter().map(|k| k.dst).collect())
    }

    /// Neighbor nodes one hop in (0x07) — the in-direction mirror of
    /// [`out_nodes`](Self::out_nodes).
    pub fn in_nodes(&self, dst: &NodeRef) -> crate::error::Result<Vec<NodeRef>> {
        Ok(self.in_edges(dst)?.into_iter().map(|k| k.src).collect())
    }

    /// Predicate-qualified neighbors (0x08/0x09): `typed_out`/`typed_in`
    /// resolved to far endpoints. Unknown predicates scan empty, like their
    /// identity counterparts.
    pub fn typed_out_nodes(
        &mut self,
        predicate: &str,
        src: &NodeRef,
    ) -> crate::error::Result<Vec<NodeRef>> {
        Ok(self
            .typed_out(predicate, src)?
            .into_iter()
            .map(|k| k.dst)
            .collect())
    }

    /// Predicate-qualified in-neighbors (0x09) — the in-direction mirror of
    /// [`typed_out_nodes`](Self::typed_out_nodes).
    pub fn typed_in_nodes(
        &mut self,
        predicate: &str,
        dst: &NodeRef,
    ) -> crate::error::Result<Vec<NodeRef>> {
        Ok(self
            .typed_in(predicate, dst)?
            .into_iter()
            .map(|k| k.src)
            .collect())
    }

    /// The out-edges of one node as a LAZY, ORDERED, repositionable stream
    /// (face 0x06) — one [`EdgeStep`] per `next()`, read straight off the entry.
    ///
    /// The eager verbs ([`out_edges`](Self::out_edges),
    /// [`out_nodes`](Self::out_nodes)) answer a `Vec` and are the right shape for
    /// "list them all". This is the shape for "walk until you find": nothing is
    /// collected, an abandoned stream stops reading, and
    /// [`find`](Iterator::find)/[`take`](Iterator::take)/
    /// [`take_while`](Iterator::take_while) cost only the entries they touch.
    ///
    /// **A step is a whole identity.** The entry IS the edge, so a hop states the
    /// endpoints, the predicate and the payload's address: the far endpoint comes
    /// off the key (one read per hop saved), and so does the predicate — every
    /// face carries it (ADR-0067).
    ///
    /// The direction is a PARAMETER, not a second API: [`inn`](Self::inn) answers
    /// the same [`EdgeStream`] over the other face, so the two compose with
    /// `chain`, `zip` and `flat_map` without an enum of shapes.
    ///
    /// **Cycle-unaware, deliberately.** A stream walks one node's face and
    /// stops; whether two paths reached "the same" node depends on the caller's
    /// filter (a tenant? a version? a kind?), so a visited set here would answer
    /// a question the stream cannot see. A hop chain is
    /// `flat_map`: `let mut here = vec![start]; for _ in 0..n { here = here.iter().flat_map(|node| g.walk(node)).map(|step| step.far).collect(); }`
    /// — and the caller that needs to stop on revisit owns that set, with its
    /// memory cost in plain sight.
    ///
    /// A fault mid-walk is parked, not raised: [`EdgeStream::error`] answers it,
    /// because an [`Iterator`] has no place to put one and a corrupt face must
    /// not read as "no more neighbours" (the rule
    /// [`SeekScan::error`](crate::model::document::SeekScan::error) states).
    pub fn out(&self, node: &NodeRef) -> EdgeStream<'_, S> {
        EdgeStream::new(
            &self.store,
            &self.members,
            self.ns,
            Direction::Out,
            node.clone(),
        )
    }

    /// The in-edges of one node as the same [`EdgeStream`] over face 0x07 —
    /// [`out`](Self::out) with the direction as its only difference.
    ///
    /// Its step's `far` is the SOURCE of the edge, and `head.dst` is the node it
    /// was started from: the mirror of the out walk, on the face the double write
    /// already materialized. No second index is involved, so "who points at this"
    /// costs exactly what "what does this point at" does.
    pub fn inn(&self, node: &NodeRef) -> EdgeStream<'_, S> {
        EdgeStream::new(
            &self.store,
            &self.members,
            self.ns,
            Direction::In,
            node.clone(),
        )
    }

    /// One hop out of `node` — the chainable spelling of [`out`](Self::out).
    ///
    /// It exists so a traversal reads as one: a hop is `g.walk(node)`, two hops
    /// is `g.walk(node).flat_map(|s| g.walk(&s.far))`, and the depth lives in
    /// the caller's iterator chain rather than in a verb. It is the same stream,
    /// not a shortcut with different semantics.
    pub fn walk(&self, node: &NodeRef) -> EdgeStream<'_, S> {
        self.out(node)
    }

    /// The predicate's vocabulary id, read-only — `None` when no edge ever
    /// carried it, so no face can hold one.
    ///
    /// [`typed_out`](Self::typed_out) needs a `&mut self` because it warms the
    /// handle's vocabulary cache on the way; this is the same probe with a
    /// throwaway cache, which is what lets a stream (holding `&self`) push a
    /// predicate down to the 0x08/0x09 prefix when [`by`](EdgeStream::by) names
    /// one. One `get` on the predicate's 0x04 face, not a walk of the edges.
    pub fn predicate_id(&self, predicate: &str) -> crate::error::Result<Option<u16>> {
        predicate_id_in(&self.store, &self.ns, predicate)
    }
}

/// Which face a hop walks: forward (0x06 / 0x08) or backward (0x07 / 0x09).
///
/// A direction is a VALUE, not a second type: [`Graph::out`] and
/// [`Graph::inn`] answer the same [`EdgeStream`], so the two directions
/// `chain`, `zip` and `flat_map` into one traversal without the consumer
/// matching on a shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Forward, source → destination: face 0x06, or 0x08 with a predicate.
    Out,
    /// Backward, destination → source: face 0x07, or 0x09 with a predicate.
    In,
}

impl Direction {
    /// The face this direction walks — predicate-qualified (0x08/0x09) when a
    /// predicate travelled with it, plain (0x06/0x07) when it did not.
    ///
    /// One decision, made in one place, so a stream cannot scan the untyped
    /// face with a predicate set (or the other way round): the two says the
    /// same thing about where the entries live.
    pub fn slot(self, predicate: Option<u16>) -> Slot {
        match (self, predicate) {
            (Direction::Out, None) => EDGE_OUT_SLOT,
            (Direction::In, None) => EDGE_IN_SLOT,
            (Direction::Out, Some(_)) => EDGE_PREDICATE_OUT_SLOT,
            (Direction::In, Some(_)) => EDGE_PREDICATE_IN_SLOT,
        }
    }
}

/// The head a traversal ENTRY states: the edge's two endpoints and its
/// predicate.
///
/// Every face carries all three now (ADR-0067): the tuple IS the entry, so a step
/// never has to admit it does not know what relation it walked. It is
/// deliberately not [`EdgeHead`] — that one names the predicate as a `String`,
/// which a key cannot carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HopHead {
    /// The edge's source endpoint.
    pub src: NodeRef,
    /// The edge's destination endpoint.
    pub dst: NodeRef,
    /// The predicate's vocabulary id — name resolution is one vocabulary read
    /// away ([`Graph::predicate_vocab_name`]).
    pub predicate: u16,
}

/// One hop, as the traversal entry itself states it.
///
/// Everything here comes from the entry's KEY: the far endpoint is the ref after
/// the scanned prefix and the predicate is the two bytes beside it. No row is
/// read to build one, so a `find` over a face costs the entries it walks and
/// nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeStep {
    /// The endpoint on the FAR side of this step: `head.dst` walking out,
    /// `head.src` walking in.
    pub far: NodeRef,
    /// The head the entry named — the edge's own direction, which is why an out
    /// step and an in step of ONE edge answer the same `head` and mirrored
    /// `far`s.
    pub head: HopHead,
}

impl EdgeStep {
    /// The edge's identity — what every other verb ([`Graph::get_edge`],
    /// [`Graph::attrs`], a retract) takes.
    pub fn key(&self) -> EdgeKey {
        EdgeKey {
            src: self.head.src.clone(),
            predicate: self.head.predicate,
            dst: self.head.dst.clone(),
        }
    }
}

/// The entry on top of an [`EdgeStream`]: its raw face key (the position
/// [`Seekable`] speaks) and the hop it decodes to.
struct FaceEntry {
    key: Vec<u8>,
    step: EdgeStep,
}

/// One node's edges, one direction, as a LAZY ORDERED CURSOR.
///
/// Built by [`Graph::out`] / [`Graph::inn`] / [`Graph::walk`]; the same type
/// answers both directions (`Direction` is a field), and
/// [`by`](Self::by) moves it onto the predicate-qualified face without
/// changing the type.
///
/// Three contracts hold it together:
///
/// - **One entry per step.** Construction puts the first entry on top (a
///   position has to exist before it can be read, the rule
///   [`SeekScan`](crate::model::document::SeekScan) follows), and each
///   [`next`](Iterator::next) hands that entry back and pulls at most one more.
///   So a consumer reads at most one entry ahead of what it was given, and
///   dropping the stream stops the scan where it stands.
/// - **Ordered by entry key.** The key is `[header][slot][scanned prefix][far
///   ref]`, so the order is the far endpoint's byte order — the address order of
///   the face, not an insertion order. That is what makes it a
///   [`Seekable`](crate::seek::Seekable) and what makes two streams over the same
///   face alignable by [`okm-query`'s operators](crate::seek).
/// - **Cycle-unaware.** The stream answers one node's edges and stops. It does
///   not dedupe and it does not remember where it has been: "the same node"
///   depends on the caller's filter, and a visited set here would price every
///   walk for a question only the caller can ask.
pub struct EdgeStream<'a, S> {
    /// The engine the face lives in — re-read by [`by`](Self::by), which
    /// reopens the scan on another face.
    store: &'a S,
    /// The declaration's width table: how a suffix becomes a ref.
    members: &'a Members,
    /// The graph's namespace, as the two header bytes every key starts with.
    ns: [u8; 2],
    /// Which face family this stream walks.
    dir: Direction,
    /// The node the walk started from: the scanned prefix, and the `near` side
    /// of every head the stream reports.
    node: NodeRef,
    /// The pushed-down predicate id, or `None` for the untyped face.
    filter: Option<u16>,
    /// The face cursor. `None` means this stream is SPENT before its first
    /// item — a predicate nobody interned has no face to walk, and that is an
    /// empty answer rather than a scan that discards everything.
    scan: Option<ScanIter>,
    /// Bytes of key before the entry's own segment: the scanned prefix's length,
    /// so decoding an entry is a slice, not a re-derived prefix.
    suffix_at: usize,
    /// The entry on top; `None` once spent.
    cur: Option<FaceEntry>,
    /// The fault that ended the walk, if one did.
    ///
    /// [`Iterator`] has no channel for it and [`Seekable`]'s verbs answer
    /// positions, so it is parked here instead of being reported as a clean
    /// end — the same rule [`SeekScan::error`](crate::model::document::SeekScan::error)
    /// states, and the reason a corrupt face cannot read as "no neighbours".
    err: Option<crate::error::Error>,
}

impl<'a, S: VirtualStorage> EdgeStream<'a, S> {
    /// Open one node's face and put its first entry on top.
    ///
    /// `pub(crate)` because the cluster layer's node form flattens the very same
    /// streams ([`crate::model::hyper::ClusterStream`]) rather than opening the
    /// faces a second time.
    pub(crate) fn new(
        store: &'a S,
        members: &'a Members,
        ns: [u8; 2],
        dir: Direction,
        node: NodeRef,
    ) -> Self {
        let mut stream = Self {
            store,
            members,
            ns,
            dir,
            node,
            filter: None,
            scan: None,
            suffix_at: 0,
            cur: None,
            err: None,
        };
        stream.reopen(None);
        stream
    }

    /// Where this stream walks — the same face family and node it was opened
    /// on, at whatever filter it carries now.
    fn prefix(&self) -> Vec<u8> {
        let mut prefix = self.ns.to_vec();
        write_slot(&mut prefix, self.dir.slot(self.filter));
        if let Some(id) = self.filter {
            prefix.extend_from_slice(&id.to_be_bytes());
        }
        self.node.write(&mut prefix);
        prefix
    }

    /// Point the cursor at (possibly) another face and re-fill it.
    ///
    /// A filter change is a different FACE, not a narrower range on the same
    /// one, so the position does not survive: the old scan is dropped and the
    /// new one starts at its own first entry.
    fn reopen(&mut self, filter: Option<u16>) {
        self.filter = filter;
        let prefix = self.prefix();
        self.suffix_at = prefix.len();
        self.cur = None;
        self.err = None;
        let end = prefix_end(&prefix);
        self.scan = Some(self.store.scan_range(&prefix, end.as_deref()));
        self.fill();
    }

    /// Put the next decodable entry on top, or clear it when the face ends.
    ///
    /// An entry whose key this face cannot carry (a suffix that does not parse by
    /// the declaration's widths, or one with bytes left over) is skipped rather
    /// than guessed at — the same fail-closed direction
    /// [`Graph::edge_keys_from`] takes, since the entry belongs to some other
    /// layout and inventing a hop from it would be worse than not seeing it.
    fn fill(&mut self) {
        self.cur = None;
        loop {
            // The pull's borrow ends here, so decoding can read `self` freely.
            let next = match self.scan.as_mut() {
                Some(scan) => scan.next(),
                None => return,
            };
            match next {
                None => return,
                Some(Err(e)) => {
                    self.err = Some(e);
                    return;
                }
                Some(Ok((key, _))) => {
                    if let Some(step) = self.decode(&key) {
                        self.cur = Some(FaceEntry { key, step });
                        return;
                    }
                }
            }
        }
    }

    /// Read one hop out of a face entry's key, turned into the edge's own
    /// direction with the node this stream started from filling the near side.
    fn decode(&self, key: &[u8]) -> Option<EdgeStep> {
        let suffix = key.get(self.suffix_at..)?;
        let (predicate, rest) = match self.filter {
            // A qualified face consumed the predicate in the scanned prefix.
            Some(id) => (id, suffix),
            // The unqualified faces put it right after the near endpoint.
            None => {
                if suffix.len() < 2 {
                    return None;
                }
                (u16::from_be_bytes([suffix[0], suffix[1]]), &suffix[2..])
            }
        };
        let (far, taken) = NodeRef::parse(rest, self.members)?;
        // The entry ends here: an identity is exactly one predicate and two
        // endpoint widths, so leftover bytes mean another layout.
        if taken != rest.len() {
            return None;
        }
        let (src, dst) = match self.dir {
            Direction::Out => (self.node.clone(), far.clone()),
            Direction::In => (far.clone(), self.node.clone()),
        };
        Some(EdgeStep {
            far,
            head: HopHead {
                src,
                dst,
                predicate,
            },
        })
    }

    /// Narrow this hop to one predicate, PUSHED DOWN onto the 0x08/0x09 prefix
    /// — one scan of that predicate's entries for this node, not a scan of the
    /// node's whole face with discards.
    ///
    /// The pushed-down form is the same [`EdgeStream`], so `by` sits anywhere a
    /// stream does: `g.out(&n).by("employs")?.take(2)`. The predicate's id
    /// comes from the graph's own vocabulary, so an unknown name is an EMPTY
    /// stream — no edge can carry a predicate nobody ever wrote — and the
    /// `Result` carries only the vocabulary read's own fault.
    ///
    /// A filter change is a new face: the position restarts at that face's
    /// first entry, and whatever the old cursor had walked is dropped with it
    /// (it was a different key space, so its keys are not bounds in this one).
    pub fn by(mut self, predicate: &str) -> crate::error::Result<Self> {
        match self.predicate_id(predicate)? {
            Some(id) => {
                self.reopen(Some(id));
                Ok(self)
            }
            // No face can hold this predicate: spent before the first item,
            // with no scan opened to read and throw away.
            None => {
                self.filter = None;
                self.scan = None;
                self.cur = None;
                Ok(self)
            }
        }
    }

    /// The predicate's vocabulary id, without warming the graph handle's cache
    /// (this stream holds `&Graph`, not `&mut Graph`, which is what lets two
    /// streams from one graph be alive at once — and compared).
    fn predicate_id(&self, predicate: &str) -> crate::error::Result<Option<u16>> {
        predicate_id_in(self.store, &self.ns, predicate)
    }

    /// The fault that ended this walk, if an engine failed mid-scan.
    ///
    /// `None` for a stream that simply ran out of entries — the distinction
    /// [`Iterator`] cannot draw, and the one a caller needs before it trusts
    /// "no more neighbours".
    pub fn error(&self) -> Option<&crate::error::Error> {
        self.err.as_ref()
    }

    /// Which direction this stream walks.
    pub fn direction(&self) -> Direction {
        self.dir
    }

    /// The node the walk started from.
    pub fn node(&self) -> &NodeRef {
        &self.node
    }

    /// The pushed-down predicate id, when [`by`](Self::by) narrowed this
    /// stream.
    pub fn predicate(&self) -> Option<u16> {
        self.filter
    }
}

impl<S: VirtualStorage> Iterator for EdgeStream<'_, S> {
    type Item = EdgeStep;

    fn next(&mut self) -> Option<EdgeStep> {
        let entry = self.cur.take()?;
        self.fill();
        Some(entry.step)
    }
}

impl<S: VirtualStorage> crate::seek::Seekable for EdgeStream<'_, S> {
    /// The face entry's RAW key — the byte string the scan's cursor speaks, so
    /// a position on one stream is directly usable as another's bound when both
    /// walk the same face family.
    type Key = Vec<u8>;

    fn key(&self) -> Option<&Vec<u8>> {
        self.cur.as_ref().map(|entry| &entry.key)
    }

    fn step(&mut self) -> bool {
        if self.cur.is_none() {
            return false;
        }
        self.fill();
        self.cur.is_some()
    }

    fn seek(&mut self, lower: &Vec<u8>) -> bool {
        match &self.cur {
            // At or past the bound: forward-only, so the position is kept.
            Some(entry) if &entry.key >= lower => return true,
            // Spent (or never opened): nothing at or after any bound remains.
            None => return false,
            Some(_) => {}
        }
        let Some(scan) = self.scan.as_mut() else {
            return false;
        };
        // The cursor moves; the entry it lands on is not consumed by the seek,
        // so `fill` puts it on top (and skips anything undecodable after it).
        if !scan.seek(lower) {
            self.cur = None;
            return false;
        }
        self.fill();
        self.cur.is_some()
    }
}

/// The predicate's id, read-only: a throwaway vocabulary cache over a shared
/// store handle.
///
/// The cache exists because the probe answers `Option<u16>` through a
/// [`Names`](okm_value::Names) door, which is the same door the write path
/// allocates through; a fresh one costs nothing on a cache miss and cannot go
/// stale, which is what a `&self` handle needs.
fn predicate_id_in<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    predicate: &str,
) -> crate::error::Result<Option<u16>> {
    let mut cache = Vocabulary::<u16>::new();
    let mut door = Faces::<_, u16>::new(store, ns, PREDICATE_VOCAB);
    cache.id_of(&mut door, predicate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members() -> Members {
        // ns 0x10 is an 8-byte key, ns 0x11 a 4-byte one: two widths, one key.
        Members::new(vec![(0x10, 8), (0x11, 4)])
    }

    fn node(ns: u16, key: u64, width: usize) -> NodeRef {
        let bytes = key.to_be_bytes();
        NodeRef::new(&ns.to_be_bytes(), &bytes[8 - width..])
    }

    fn person(id: u64) -> NodeRef {
        node(0x10, id, 8)
    }

    fn org(id: u32) -> NodeRef {
        node(0x11, id as u64, 4)
    }

    #[test]
    fn a_ref_wire_is_the_ns_then_the_pkey_with_no_length() {
        let wire = person(7).wire();
        assert_eq!(wire.len(), 2 + 8);
        assert_eq!(&wire[..2], &0x10u16.to_be_bytes());
        assert_eq!(&wire[2..], &7u64.to_be_bytes());
        let (back, n) = NodeRef::parse(&wire, &members()).unwrap();
        assert_eq!(n, wire.len());
        assert_eq!(back, person(7));
        // A ns the declaration does not name is not parseable at all.
        let foreign = NodeRef::new(&0x99u16.to_be_bytes(), &[1, 2, 3, 4]);
        assert!(NodeRef::parse(&foreign.wire(), &members()).is_none());
        // The width is the declaration's, so a 4-byte key reads 4 bytes.
        let small = org(9);
        assert_eq!(small.wire().len(), 2 + 4);
        assert_eq!(NodeRef::parse(&small.wire(), &members()).unwrap().0, small);
    }

    #[test]
    fn an_edge_key_is_the_tuple_and_parses_by_width() {
        let key = EdgeKey {
            src: person(1),
            predicate: 0x0102,
            dst: org(9),
        };
        let wire = key.wire();
        // No length prefix anywhere (ADR-0067):
        // [src ns 2][src pkey 8][predicate 2][dst ns 2][dst pkey 4]
        assert_eq!(wire.len(), 10 + 2 + 6);
        assert_eq!(&wire[..2], &0x10u16.to_be_bytes());
        assert_eq!(&wire[2..10], &1u64.to_be_bytes());
        assert_eq!(&wire[10..12], &0x0102u16.to_be_bytes());
        assert_eq!(&wire[12..14], &0x11u16.to_be_bytes());
        assert_eq!(&wire[14..], &9u32.to_be_bytes());

        let (back, n) = EdgeKey::parse(&wire, &members()).expect("parse");
        assert_eq!(n, wire.len());
        assert_eq!(back, key);
        // Trailing bytes belong to the next segment; the parse consumes exactly
        // its own.
        let mut longer = wire.clone();
        longer.extend_from_slice(&[0xEE, 0xEE]);
        assert_eq!(EdgeKey::parse(&longer, &members()).unwrap().1, wire.len());
        // Nothing shorter parses.
        assert!(EdgeKey::parse(&wire[..wire.len() - 1], &members()).is_none());
    }

    #[test]
    fn an_edge_key_satisfies_the_key_contract_it_can() {
        // No single width: the identity's shape is the declaration's.
        assert_eq!(EdgeKey::KEY_LEN, 0);
        assert!(EdgeKey::FIELDS.is_empty());
        assert_eq!(EdgeKey::prefix_width(&[]), 0);
        let key = EdgeKey {
            src: person(1),
            predicate: 1,
            dst: org(2),
        };
        assert_eq!(key.encode(), key.wire());
    }

    #[test]
    fn the_member_table_answers_widths_by_ns() {
        let m = members();
        assert_eq!(m.key_len(0x10), Some(8));
        assert_eq!(m.key_len(0x11), Some(4));
        assert_eq!(m.key_len(0x12), None);
        assert!(m.contains(0x10));
        assert!(!m.contains(0x12));
    }
}
