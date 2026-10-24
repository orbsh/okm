//! Secondary indexes (access methods) and the [`Document`] payload contract.
//!
//! Index entry layout (ADR-0005/0006, ADR-0016): key
//! `[ns 2B][slot 2B][index fields BE][key prefix]`, value = the includes
//! segment (raw payload-field encodings, empty when no `includes`).
//!
//! - **Index fields** come from the document payload, encoded by name in the
//!   index's declared order — the sort key is payload data.
//! - **Key prefix** is the tail segment: the full primary-key encoding by
//!   default, or a declared declaration-order prefix of the key struct
//!   (`key(...)` clause). A truncated prefix makes the entry itself a list
//!   encoding: `fields(name) key(user_id)` over a `(user_id, timestamp)`
//!   table yields `[name][user_id]` — one prefix scan returns the user's
//!   whole list (friends, timeline) without going through the primary key.
//!   The tail is always decodable from the last bytes of the entry key.
//!
//! The primary table is slot 0x0000: `[ns 2B][0x0000][key]`, value = TLV
//! payload. Access methods live in the index segment (0x1), counters by
//! attribute order inside one item (never reused — hole discipline same
//! as ns IDs, ADR-0005).

use crate::engine::storage::VirtualStorage;
use crate::model::key::{KeyEncode, PrefixKey};

/// Slot type: either a **fixed face** (one byte, 0x00–0x0F — a family with a
/// single face and nothing to enumerate) or a **counting family** (two bytes:
/// the family number in the first byte's high nibble plus a 12-bit counter,
/// ADR-0030). Big-endian in the key, and the width follows from the value, so
/// a family-prefix scan is exact without consulting a table.
pub type Slot = u16;

/// Family numbers (the high nibble of a counting family's first byte): entry
/// kinds whose instances are enumerated. An enumeration, not a space
/// allocation.
pub mod segment {
    /// Declared vocabularies (ADR-0032), the label → id direction: the key is
    /// the label, the value the id. Counter = the vocabulary's own number
    /// (declaration order among the document's `Vocab<N>` fields) — ONE number
    /// per vocabulary, because the DIRECTION is the family.
    pub const VOCAB_LABEL: u16 = 0x1;
    /// Declared vocabularies, the id → label direction: same counter, other
    /// family. `vocab_faces(i)` pairs the two with the cursor below.
    pub const VOCAB_ID: u16 = 0x2;
    /// Declared indexes (declaration-order counter).
    pub const INDEX: u16 = 0x3;
    /// Declared reduces (independent declaration-order counter).
    pub const REDUCE: u16 = 0x4;
    /// Junctions (one one-way entry per endpoint ns, ADR-0015).
    pub const JUNCTION: u16 = 0x5;
}

/// Assemble a counting family's slot from its family number and counter.
pub const fn slot(family: u16, counter: u16) -> Slot {
    (family << 12) | (counter & 0x0FFF)
}

/// Highest fixed-face number — the whole one-byte space. A fixed face carries
/// no counter, and keeping the space at high nibble 0 is what makes
/// "first byte's high nibble is 0" a sufficient delimiter.
pub const FIXED_FACE_MAX: Slot = 0x0F;

/// Write a slot in its own width: one byte for a fixed face, two for a
/// counting family (ADR-0030). The panic arm is a layout error, not a
/// runtime risk: every slot comes from a constant or a declaration order.
pub fn write_slot(buf: &mut Vec<u8>, slot: Slot) {
    if slot <= FIXED_FACE_MAX {
        buf.push(slot as u8);
    } else {
        assert!(
            slot >> 12 != 0,
            "slot 0x{slot:04X} is neither a fixed face (0x00–0x0F) nor a counting family (family nibble != 0)"
        );
        buf.extend_from_slice(&slot.to_be_bytes());
    }
}

/// The byte width [`write_slot`] uses for this slot.
pub const fn slot_len(slot: Slot) -> usize {
    if slot <= FIXED_FACE_MAX { 1 } else { 2 }
}

/// A collection's primary keys (document collection or edge collection):
/// fixed face 0x00.
pub const PRIMARY_SLOT: Slot = 0x00;
/// obj dynamic segment (ADR-0012), per-row undeclared fields: fixed face 0x01.
pub const DYNAMIC_SLOT: Slot = 0x01;
/// Field-name vocabulary, number → name (ADR-0012): fixed face 0x02. It also
/// carries the pair's allocation cursor as its FACE-LEVEL SINGLETON — the bare
/// prefix `[ns][0x02]`, no suffix (ADR-0030's rule, applied by ADR-0032);
/// `obj_vocab::cursor_key` builds that key.
pub const FIELD_VOCAB_ID_SLOT: Slot = 0x02;
/// Field-name vocabulary, name → number (ADR-0012): fixed face 0x03.
pub const FIELD_VOCAB_NAME_SLOT: Slot = 0x03;
/// Predicate vocabulary, number → name: fixed face 0x04. A collection's token
/// vocabulary BEYOND field names — a graph's predicates, a node collection's
/// kind tokens (ADR-0017 §3). A pair of its own, never a band inside 0x02/0x03:
/// two allocators in one id space hand the same id to two different names
/// silently. Carries its pair's allocation cursor at the bare prefix
/// `[ns][0x04]`, exactly like 0x02 — same rule, same derived key.
pub const PREDICATE_VOCAB_ID_SLOT: Slot = 0x04;
/// Predicate vocabulary, name → number: fixed face 0x05 (the mirror of
/// [`PREDICATE_VOCAB_ID_SLOT`]).
pub const PREDICATE_VOCAB_NAME_SLOT: Slot = 0x05;
/// Slot base available to `#[ok_index]` declaration-order allocation —
/// counters start at 1 (0 reserved), so 4095 declared indexes per collection.
pub const DECLARED_SLOT_BASE: Slot = slot(segment::INDEX, 1);
/// Slot base for `#[ok_reduce]` declarations — its own counter, 4095 per
/// collection.
pub const REDUCE_SLOT_BASE: Slot = slot(segment::REDUCE, 1);
/// Slot base for junction discriminators (`#[ok_junction(n)]`, ADR-0015: one
/// one-way entry per endpoint ns; counter = discriminator << 1 | direction).
pub const JUNCTION_SLOT_BASE: Slot = slot(segment::JUNCTION, 0);

/// Encoded form of a function-index result (ADR-0005, function-index
/// regime): what the declared function returns must land in the index
/// entry's computed-field segment in a well-defined sort order.
///
/// - `String` → raw UTF-8 bytes: sort order IS byte-wise lexicographic
///   (the text-first regime). No length prefix — a length prefix would
///   sort by length before bytes and destroy vocabulary order; exact
///   matching is resolved by the primary-key tail + fetch-back.
/// - Unsigned integers → big-endian bytes: sort order IS numeric order.
///
/// A function returning anything else fails to compile at the generated
/// `KvIndex` impl (the trait bound names this trait explicitly).
pub trait IndexFuncResult {
    /// Append the index-segment encoding of this value.
    fn encode_index(&self, buf: &mut Vec<u8>);
}

impl IndexFuncResult for String {
    fn encode_index(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.as_bytes());
    }
}

macro_rules! index_func_uint {
    ($($t:ty),*) => {$(
        impl IndexFuncResult for $t {
            fn encode_index(&self, buf: &mut Vec<u8>) {
                buf.extend_from_slice(&self.to_be_bytes());
            }
        }
    )*};
}

index_func_uint!(u8, u16, u32, u64);

/// The set of index entries one function-index invocation produces.
/// Single values yield one entry (the classic function index); iterators
/// of values (`Vec<String>`, token streams) yield one entry each — the
/// multi-entry regime (inverted index, multi-valued fields). A type
/// cannot implement both arms: `Vec<V>` never implements
/// `IndexFuncResult`, so the two impls do not overlap.
pub trait IndexFuncValues {
    /// Each element is one entry's index-segment encoding.
    fn func_values(self) -> Vec<Vec<u8>>;
}

macro_rules! index_func_values_single {
    ($($t:ty),*) => {$(impl IndexFuncValues for $t {
        fn func_values(self) -> Vec<Vec<u8>> {
            let mut buf = Vec::new();
            IndexFuncResult::encode_index(&self, &mut buf);
            vec![buf]
        }
    })*};
}
index_func_values_single!(String, u8, u16, u32, u64);

impl<V: IndexFuncResult> IndexFuncValues for Vec<V> {
    fn func_values(self) -> Vec<Vec<u8>> {
        self.into_iter()
            .map(|v| {
                let mut buf = Vec::new();
                v.encode_index(&mut buf);
                buf
            })
            .collect()
    }
}

/// Value-side payload contract (ADR-0004/0006): a document = identity (its key)
/// plus payload fields, laid out as two segments behind a header —
/// `[version u8][hot_len u16 BE][hot segment][cold segment]`:
///
/// - **Hot segment**: fixed-width fields in declaration order, contiguous —
///   O(1) per-field offsets, no frame headers.
/// - **Cold segment**: variable-width fields (`String`, `VarInt`) as TLV —
///   one `[tag u8][len varint][value]` frame per field, `tag` = the
///   field's declaration index over ALL payload fields.
///
/// Schema evolution is append-only: new fields are added at the tail of
/// their segment (never inserted between existing fields), so older
/// payloads decode with the new fields taking their declared defaults
/// (`#[ok_default(expr)]`, else `Default::default()`); the header version
/// bumps to record the change, and payloads from a newer version than the
/// schema are rejected.
///
/// Generated by `#[derive(DocumentEncode)]`; the declaration is the single
/// source for value encoding, index slots, and snapshot columns.
pub trait Document: Sized + Clone {
    /// Embedded (Ref/Refs) field dereference (ADR-0012 embedded milestone): after the
    /// parent payload decodes, fetch each `Unit` field's child document
    /// by its stored key and backfill `value`. Default no-op (no embedded
    /// fields). Derive-generated; failures leave `value: None`.
    fn __okm_embed_deref(&mut self, _store: &dyn VirtualStorage) {}
    /// Embedded-field (Ref/Refs) write entries: `(child pkey, child payload)` pairs
    /// for fields carrying `value: Some(_)`. Default empty.
    fn __okm_embed_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        Vec::new()
    }
    /// Embedded-field (Ref/Refs) keys previously pointed at (from a decoded OLD
    /// document) — put's overwrite pass releases stale references.
    fn __okm_embed_keys(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// The table's NAME (`#[ok_name("...")]`), beside the ns it belongs to.
    /// The ns is the store's address; the name is the identity a table carries
    /// OUT of it — the export's catalog column and the import-side lookup.
    /// Empty = unnamed: an export refuses it rather than spelling the struct.
    const TABLE_NAME: &'static str = "";
    /// Whether this document declares any `Vocab<N>` field (ADR-0032). `put`
    /// reads it to choose between encoding the caller's document directly and
    /// taking a mutable copy for the intern step — tables without categorical
    /// values pay only the constant.
    const HAS_VOCAB: bool = false;
    /// Resolve every `Vocab<N>` field's label to its id in that FIELD's data
    /// vocabulary (ADR-0032), writing the vocabulary entries through `store`.
    /// Called by `put` before encoding — and by `save_with_prev_into` through
    /// `BatchStore`, so vocabulary growth, the row and its faces land in one
    /// batch. Default no-op (no categorical fields). Interning past 255 distinct
    /// labels in one field is a loud error, never a wrap.
    fn __okm_intern(&mut self, _store: &mut dyn VirtualStorage, _header: &[u8]) {}
    /// Resolve every `Vocab<N>` field's decoded id back into its label
    /// (ADR-0032), so the document API and `to_map` see names. Called by the
    /// read path after `__okm_embed_deref`; default no-op (no categorical
    /// fields). A missing vocabulary entry leaves the value unresolved rather
    /// than inventing a name.
    fn __okm_deref_vocab(&mut self, _store: &dyn VirtualStorage) {}
    /// document -> map: every declared field lifted into a run-time
    /// DynamicValue (ADR-0012 document-map bridge; derive-generated).
    fn to_map(&self) -> std::collections::BTreeMap<String, crate::model::obj_dynamic::DynamicValue>;
    /// map -> document: matched fields assigned from DynamicValue; missing
    /// fields fall back to `#[ok_default]`/Default (derive-generated).
    fn from_map(map: &std::collections::BTreeMap<String, crate::model::obj_dynamic::DynamicValue>) -> Self
    where
        Self: Sized;
    /// Identity type this document hangs off (from `#[ok_ref(...)]`).
    type Key: KeyEncode;
    /// Layout version written into the payload header (`#[ok_layout(version)]`,
    /// default 1). Decode accepts `<= LAYOUT_VERSION`, rejects newer.
    const LAYOUT_VERSION: u8 = 1;
    /// Payload field name → width table, declaration order (snapshot columns).
    const PAYLOAD_FIELDS: &'static [(&'static str, usize)];
    /// Payload field descriptors, declaration order — single source shared
    /// with PAYLOAD_FIELDS plus the primitive kind (Arrow schema, column
    /// builders, snapshot tooling; ADR-0007).
    const FIELDS: &'static [crate::model::field::FieldDesc] = &[];
    /// Const-constructible field defaults, name-keyed (literal
    /// `#[ok_default]` only). Consumed by `CollectionSchema::of` to fill
    /// `FieldSchema::default` — the dynamic reader's version-migration
    /// data. Empty when no field declares a literal default.
    const DEFAULTS: &'static [(&'static str, crate::model::field::DefaultValueConst)] = &[];
    /// Per-field application contracts (`#[ok_len(N)]` on `Vector<T>`
    /// fields): `(field name, expected element count)`. Exported into
    /// `FieldSchema::expect_len` so the dynamic reader enforces the same
    /// check the Rust decoder generates. Default empty.
    const FIELD_CONTRACTS: &'static [(&'static str, usize)] = &[];
    /// Byte width of the hot segment at THIS schema version: the fixed-
    /// width fields in declaration order, concatenated. The decode-side
    /// split point of the two segments (hot walk ends, cold TLV walk
    /// begins). Empty hot segment = 0.
    const HOT_WIDTH: usize = 0;
    /// TLV payload encoding of the attribute fields.
    fn encode_payload(&self) -> Vec<u8>;
    /// Decode payload; `b` holds only the two-segment region (no key bytes).
    fn decode_payload(b: &[u8]) -> Self;
    /// All declared access methods' `(entry key, entry value)` pairs for
    /// `key` + `document`, in slot order. Index entries depend on the payload
    /// (indexed and includes fields live there), so the document is required —
    /// put and delete are both document-shaped. Generated; lets Collection cover
    /// every declared index without a runtime registry (the declaration IS
    /// the registry).
    fn index_entries(key: &Self::Key, document: &Self, ns: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)>;

    /// Cross-document reduce hook (see [`crate::model::reduce`]): apply this
    /// document to every declared `#[ok_reduce]` group. Default no-op —
    /// only documents with reduce declarations override it.
    fn __okm_apply_reduces<S: VirtualStorage>(
        _store: &mut S,
        _key: &Self::Key,
        _row: &Self,
        _ns: &[u8],
        _add: bool,
    ) {
    }

    /// Subscribe emit hook (see [`crate::subscribe`]): send this document's
    /// write-path event into its declared channel. Default no-op — only
    /// documents carrying `#[ok_subscribe]` override it. Best-effort by
    /// contract (try_send); never blocks or fails the write. `_epoch` is
    /// the emitting collection's monotonic write-batch counter.
    fn __okm_emit_event(
        _op: crate::subscribe::Op,
        _epoch: u64,
        _key: &Self::Key,
        _row: &Self,
    ) {
    }

    /// Assembly-point constructor: builds the document's `Collection`
    /// binding this document type to its `#[ok_ref]` key. The key type
    /// never appears at the call site — it is already pinned by
    /// `Self::Key`.
    fn collection<S: VirtualStorage>(store: S) -> crate::model::document::Collection<S, Self::Key, Self> {
        crate::model::document::Collection::new(store)
    }

    /// Slots reserved by `deprecated` index declarations (ADR-0005):
    /// entries under `[ns][slot]` for these slots are stale leftovers
    /// from before the declaration was deprecated — never written by the
    /// current code, cleared by `Collection::prune_deprecated_slots`. Default
    /// empty (no deprecated declarations).
    const DEPRECATED_SLOTS: &'static [Slot] = &[];

    /// The namespace prefix this document's collection lives under, encoded and
    /// ready to prepend (`[ns 2B]` big-endian). Declared via `#[ok_ns(N)]`
    /// on the document struct — the document is the collection's declaration point (its
    /// `#[ok_ref]` pins the key type, so `Collection<S, K, R>` is fully
    /// determined by the document), never hand-filled at the assembly site
    /// (ADR-0002: the ns vocabulary is code). A key type carries no ns of
    /// its own: the same key shape may legitimately serve several documents /
    /// tables, each with its own declared ns. Default = empty (no ns
    /// declared — a layout-only document that never materializes a collection).
    const NS_PREFIX: &'static [u8] = &[];

    /// The same ns as a NUMBER: `NS_PREFIX` decoded (`None` = no ns declared).
    /// A reader that reasons about collections rather than keys uses this one —
    /// a graph's `#[ok_nodes]` lists document types and takes their ns from
    /// here, so a membership set can never disagree with the table's own
    /// `#[ok_ns]`. Both constants come from that one declaration.
    const NS: Option<u16> = None;

    /// The collection's partition id (ADR-0014 §5): `Some(N)` prepends a
    /// 2-byte escape segment `[0xFF][N]` before the ns header — physical
    /// partition routing (Fjall) and workload isolation in the key space.
    /// The 0xFF first byte is a reserved escape: legal ns headers
    /// (big-endian u16 constrained by the ns vocabulary) never start with
    /// it, so partitioned and unpartitioned keys are structurally disjoint
    /// — no numbering discipline needed. `None` (default) = no segment at
    /// all: zero key-encoding cost for tables without partition needs.
    /// The id is a compile-time constant on the type — decoding always
    /// knows the layout. Engines without partition semantics ignore the
    /// physical split; the key encoding is identical everywhere.
    /// Declared via `#[ok_partition(N)]`; bare `#[ok_partition]` /
    /// `#[ok_partition(0)]` are rejected by the derive.
    const PARTITION_ID: Option<u8> = None;
    /// Encoded partition segment, ready to prepend (`[0xFF][part 1B]`).
    /// Empty when `PARTITION_ID` is `None`. Consumed by
    /// `Collection::primary_key` / index entry assembly before the ns header.
    const PARTITION_PREFIX: &'static [u8] = &[];
}

/// One access method over a collection. Implemented by generated marker
/// structs (`#[derive(DocumentEncode)]` + `#[ok_index(...)]`); slots derive
/// from the collection's ns — indexes never take manual namespace IDs.
pub trait KvIndex {
    type Key: KeyEncode;
    /// The document type this access method reads its index fields from.
    type Document: Document<Key = Self::Key>;
    /// Item-local slot, allocated by attribute order (index segment,
    /// 0x3001, 0x3002, …; primary = 0x0000). 2 bytes in the entry header,
    /// big-endian (ADR-0016).
    const SLOT: Slot;
    /// Indexed payload fields, in sort order.
    const FIELDS: &'static [&'static str];
    /// Covering payload fields carried in the entry value (may be empty).
    /// `includes` makes the scan self-sufficient — a materialized view for
    /// high-fanout queries (ADR-0006), never a default optimization. They
    /// live in the value: they do not participate in the sort order.
    const INCLUDES: &'static [&'static str];
    /// Key fields forming the tail prefix — a declaration-order prefix of
    /// the key struct (empty = the full primary-key encoding).
    const KEY_PREFIX: &'static [&'static str];
    /// Function-index function path (ADR-0005, function-index regime);
    /// empty = plain field index. The generated impl calls this path with
    /// `&document` and encodes the result via `IndexFuncResult` (sort order =
    /// the result encoding's order). The query side calls the same path on
    /// its probe value — one declaration drives both encode and scan.
    const FUNC: &'static str = "";

    /// Encode the named fields in `names` order. Generated impls source
    /// every name from the document payload; hand impls may read identity
    /// fields off `key` instead (the hook receives both).
    fn encode_named(key: &Self::Key, document: &Self::Document, names: &[&str], buf: &mut Vec<u8>);

    /// Encoded index-field segment (the sort key, after the header).
    fn fields_bytes(key: &Self::Key, document: &Self::Document) -> Vec<u8> {
        let mut buf = Vec::new();
        Self::encode_named(key, document, Self::FIELDS, &mut buf);
        buf
    }

    /// Byte width of the key-prefix tail segment.
    fn key_prefix_width() -> usize {
        if Self::KEY_PREFIX.is_empty() {
            Self::Key::KEY_LEN
        } else {
            Self::Key::prefix_width(Self::KEY_PREFIX)
        }
    }

    /// Encoded key-prefix tail segment (full key encoding when
    /// `KEY_PREFIX` is empty).
    fn key_prefix_bytes(key: &Self::Key) -> Vec<u8> {
        if Self::KEY_PREFIX.is_empty() {
            key.encode()
        } else {
            let mut buf = Vec::with_capacity(Self::key_prefix_width());
            key.encode_prefix_named(&mut buf, Self::KEY_PREFIX);
            buf
        }
    }

    /// Full entry key: `[ns 2B][slot][index fields][key prefix]` — an access
    /// method's slot names a counting family, so it is two bytes here
    /// (ADR-0030); the collection's ns allocation is untouched by how many
    /// indexes exist (ADR-0005).
    fn entry_key(ns_prefix: &[u8], key: &Self::Key, document: &Self::Document) -> Vec<u8> {
        let fb = Self::fields_bytes(key, document);
        let kp = Self::key_prefix_bytes(key);
        let mut buf = Vec::with_capacity(ns_prefix.len() + slot_len(Self::SLOT) + fb.len() + kp.len());
        buf.extend_from_slice(ns_prefix);
        write_slot(&mut buf, Self::SLOT);
        buf.extend_from_slice(&fb);
        buf.extend_from_slice(&kp);
        buf
    }

    /// Entry value: the includes segment (raw payload-field encodings,
    /// concatenation in `INCLUDES` order; empty when no includes).
    fn entry_value(key: &Self::Key, document: &Self::Document) -> Vec<u8> {
        let mut buf = Vec::new();
        if !Self::INCLUDES.is_empty() {
            Self::encode_named(key, document, Self::INCLUDES, &mut buf);
        }
        buf
    }

    /// Decoded form of [`entry_value`](Self::entry_value): the covered
    /// payload fields, in `INCLUDES` order, as a generated struct
    /// (`__OkmCovered_{Row}_{index}`). `()` when the declaration covers
    /// nothing — an index without `includes` has an empty value segment, so
    /// there is nothing to decode and a scan over it is a key scan.
    type Covered;

    /// Decode one entry's includes segment (the entry VALUE) into
    /// [`Covered`](Self::Covered) — the read half of `includes`, and the only
    /// door that answers without the primary table.
    ///
    /// The segment is a bare concatenation of payload-field encodings: no
    /// tags, no length prefixes, located by width alone — fixed-width fields
    /// cut at their declared width, a trailing variable-width field takes the
    /// remainder (the same order rule `FIELDS` follows, enforced at
    /// declaration time). `b` must be exactly one entry value: a short head or
    /// leftover bytes is a layout error and panics (never caller input).
    fn decode_covered(b: &[u8]) -> Self::Covered;

    /// All entries this access method produces for one document: plain and
    /// single-value function indexes yield one `(key, value)` pair;
    /// multi-value function indexes yield one pair per produced value —
    /// the write side (`Collection::put` / `purge` via `index_entries`) just
    /// iterates. Each entry shares the same includes value.
    ///
    /// Partial indexes (`where(path)` declaration clause) short-circuit
    /// here: `admits` is consulted once per document, before any entry is
    /// built. Nothing else in the write path can create an entry, so the
    /// filter is complete by construction.
    fn entry_pairs(ns_prefix: &[u8], key: &Self::Key, document: &Self::Document) -> Vec<(Vec<u8>, Vec<u8>)> {
        if !Self::admits(document) {
            return Vec::new();
        }
        vec![(Self::entry_key(ns_prefix, key, document), Self::entry_value(key, document))]
    }

    /// Partial-index predicate — the `where(path)` clause of an
    /// `#[ok_index]` declaration. `false` = this document contributes NO
    /// entries to this access method (Postgres-style partial index).
    /// Default `true`: a full index.
    ///
    /// Evaluated once per document per generation pass, by `entry_pairs`
    /// — the single place entries are produced for put, delete AND
    /// save_into. Removal therefore regenerates exactly the write set as
    /// long as the predicate is pure: a predicate reading a clock or
    /// external state makes delete compute a different set and leaves
    /// dangling entries (same purity contract as a `func` path).
    ///
    /// Scans never consult it — a partial index is simply sparser. A scan
    /// over one is complete only for queries whose condition implies the
    /// predicate; nothing enforces that, the declaration states which rows
    /// are in the index and the caller owns the query.
    fn admits(_document: &Self::Document) -> bool {
        true
    }

    /// Scan prefix for a leftmost-prefix match over the index fields:
    /// header + the caller-side encoding of the leading index fields
    /// (e.g. `7u32.to_be_bytes()` for a u32 field; empty slice = whole
    /// index). Must not exceed the index-field segment width.
    fn entry_prefix(ns_prefix: &[u8], encoded: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ns_prefix.len() + slot_len(Self::SLOT) + encoded.len());
        buf.extend_from_slice(ns_prefix);
        write_slot(&mut buf, Self::SLOT);
        buf.extend_from_slice(encoded);
        buf
    }
}

/// Leftmost-prefix scan over an index: returns each entry's decoded key
/// prefix. The key prefix is the tail segment of the entry key, so it is
/// recovered from the last `key_prefix_width()` bytes — full key when
/// `KEY_PREFIX` is empty, truncated identity otherwise (trailing fields
/// are zero-filled, use only the prefix fields).
///
/// Prefix semantics: the probe is a byte prefix, so a stored value that
/// EXTENDS it matches (`"alpha"` probes `"alpha"` and `"alphabet"`). One
/// family is not a match and is dropped: a probe that ran past the
/// entry's field segment and was satisfied by primary-key TAIL bytes
/// (stored `"ad"` + tail `0x31…` matching the probe `"ad1"` — values mix
/// letters and digits, so this data is legal). The field segment is
/// bounded by the tail's known width, so the test is one length compare:
/// a remainder shorter than the tail means the stored value is shorter
/// than the probe and cannot start with it.
pub fn scan_index<S: VirtualStorage, I: KvIndex>(
    store: &S,
    ns_prefix: &[u8],
    encoded: &[u8],
) -> Vec<PrefixKey<I::Key>> {
    scan_index_filtered::<S, I>(store, ns_prefix, encoded, false)
}

/// Equality scan over an index: only entries whose field segment is
/// EXACTLY the probe — the probe consumes the whole segment, so nothing
/// of it is carried by the tail. This is exact matching without reading
/// the row: a stored value longer than the probe matches a prefix scan
/// but not this one, and the tail-spill case is excluded by the same
/// length test. The probe must name EVERY field of the index's data
/// segment (a partial probe describes a group — use [`scan_index`]).
pub fn scan_index_exact<S: VirtualStorage, I: KvIndex>(
    store: &S,
    ns_prefix: &[u8],
    encoded: &[u8],
) -> Vec<PrefixKey<I::Key>> {
    scan_index_filtered::<S, I>(store, ns_prefix, encoded, true)
}

fn scan_index_filtered<S: VirtualStorage, I: KvIndex>(
    store: &S,
    ns_prefix: &[u8],
    encoded: &[u8],
    exact: bool,
) -> Vec<PrefixKey<I::Key>> {
    let p = I::entry_prefix(ns_prefix, encoded);
    let taken = I::key_prefix_width();
    let kl = I::Key::KEY_LEN;
    store
        .scan_suffix(&p)
        .iter()
        .filter_map(|suffix| {
            // The tail's width is known, so the field segment's length is
            // `suffix.len() - taken` (plus the probe): below the tail width
            // the probe was satisfied by tail bytes — not a match. Exact
            // additionally wants nothing left after the probe.
            let matched = if exact { suffix.len() == taken } else { suffix.len() >= taken };
            if !matched {
                return None;
            }
            let start = suffix.len() - taken;
            let decoded = if taken == kl {
                I::Key::decode(&suffix[start..])
            } else {
                // Truncated identity: zero-fill past the prefix boundary —
                // trailing fields are garbage by contract (PrefixKey).
                let mut buf = vec![0u8; kl];
                buf[..taken].copy_from_slice(&suffix[start..]);
                I::Key::decode(&buf)
            };
            Some(PrefixKey { decoded, taken })
        })
        .collect()
}
