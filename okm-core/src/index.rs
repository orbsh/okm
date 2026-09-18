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

use crate::storage::VirtualStorage;
use crate::key::{KeyEncode, PrefixKey};

/// Slot type: 4-bit segment (high nibble, entry kind) + 12-bit in-segment
/// counter (ADR-0016). Big-endian in the key.
pub type Slot = u16;

/// Segment numbers (the high nibble of a slot): entry kinds, structurally
/// dispatched by `slot >> 12`. An enumeration, not a space allocation.
pub mod segment {
    /// Document-self: primary, dynamic segment, dictionaries, buffer.
    pub const SELF: u16 = 0x0;
    /// Declared indexes (declaration-order counter).
    pub const INDEX: u16 = 0x1;
    /// Declared reduces (independent declaration-order counter).
    pub const REDUCE: u16 = 0x2;
    /// Junctions (one one-way entry per endpoint ns, ADR-0015/0016).
    pub const JUNCTION: u16 = 0x3;
}

/// Assemble a slot from its segment and in-segment counter.
pub const fn slot(segment: u16, counter: u16) -> Slot {
    (segment << 12) | (counter & 0x0FFF)
}

/// Slot reserved for a collection's primary keys inside its ns segment.
pub const PRIMARY_SLOT: Slot = slot(segment::SELF, 0);
/// obj dynamic segment (ADR-0012): per-document undeclared fields.
pub const DYNAMIC_SLOT: Slot = slot(segment::SELF, 1);
/// Field-name dictionary, number → name (ADR-0012).
pub const DICT_ID_SLOT: Slot = slot(segment::SELF, 2);
/// Field-name dictionary, name → number (ADR-0012).
pub const DICT_NAME_SLOT: Slot = slot(segment::SELF, 3);
/// Slot base available to `#[ok_index]` declaration-order allocation
/// (ADR-0016: counters start at 1; 0 is the primary).
pub const DECLARED_SLOT_BASE: Slot = slot(segment::INDEX, 1);
/// Slot base for `#[ok_reduce]` declarations — independent of the index
/// counter (ADR-0016 removed the chained coupling).
pub const REDUCE_SLOT_BASE: Slot = slot(segment::REDUCE, 1);
/// Slot base for junction discriminators (`#[ok_junction(n)]`,
/// ADR-0015/0016: one one-way entry per endpoint ns).
pub const JUNCTION_SLOT_BASE: Slot = slot(segment::JUNCTION, 0);

/// Encoded form of a function-index result (ADR-0005, function-index
/// regime): what the declared function returns must land in the index
/// entry's computed-field segment in a well-defined sort order.
///
/// - `String` → raw UTF-8 bytes: sort order IS byte-wise lexicographic
///   (the text-first regime). No length prefix — a length prefix would
///   sort by length before bytes and destroy dictionary order; exact
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
///   one `[tag u8][len u32 BE][value]` frame per field, `tag` = the
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
    fn __okm_embed_deref(&mut self, store: &dyn VirtualStorage) {}
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
    /// document -> map: every declared field lifted into a run-time
    /// DynamicValue (ADR-0012 document-map bridge; derive-generated).
    fn to_map(&self) -> std::collections::BTreeMap<String, crate::obj_dynamic::DynamicValue>;
    /// map -> document: matched fields assigned from DynamicValue; missing
    /// fields fall back to `#[ok_default]`/Default (derive-generated).
    fn from_map(map: &std::collections::BTreeMap<String, crate::obj_dynamic::DynamicValue>) -> Self
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
    const FIELDS: &'static [crate::field::FieldDesc] = &[];
    /// Const-constructible field defaults, name-keyed (literal
    /// `#[ok_default]` only). Consumed by `TableSchema::of` to fill
    /// `FieldSchema::default` — the dynamic reader's version-migration
    /// data. Empty when no field declares a literal default.
    const DEFAULTS: &'static [(&'static str, crate::field::DefaultValueConst)] = &[];
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

    /// Cross-document reduce hook (see [`crate::reduce`]): apply this
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
    fn collection<S: VirtualStorage>(store: S) -> crate::document::Collection<S, Self::Key, Self> {
        crate::document::Collection::new(store)
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
    /// (ADR-0002: the ns dictionary is code). A key type carries no ns of
    /// its own: the same key shape may legitimately serve several documents /
    /// tables, each with its own declared ns. Default = empty (no ns
    /// declared — a layout-only document that never materializes a collection).
    const NS_PREFIX: &'static [u8] = &[];

    /// The collection's partition id (ADR-0014 §5): `Some(N)` prepends a
    /// 2-byte escape segment `[0xFF][N]` before the ns header — physical
    /// partition routing (Fjall) and workload isolation in the key space.
    /// The 0xFF first byte is a reserved escape: legal ns headers
    /// (big-endian u16 constrained by the ns dictionary) never start with
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
    /// 0x1001, 0x1002, …; primary = 0x0000). 2 bytes in the entry header,
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

    /// Full entry key: `[ns 2B][slot 2B][index fields][key prefix]` —
    /// the slot's segment dispatches entry kinds *within* the collection's
    /// ns segment (ADR-0016); the collection's ns allocation is untouched
    /// by how many indexes exist (ADR-0005).
    fn entry_key(ns_prefix: &[u8], key: &Self::Key, document: &Self::Document) -> Vec<u8> {
        let fb = Self::fields_bytes(key, document);
        let kp = Self::key_prefix_bytes(key);
        let mut buf = Vec::with_capacity(ns_prefix.len() + 2 + fb.len() + kp.len());
        buf.extend_from_slice(ns_prefix);
        buf.extend_from_slice(&Self::SLOT.to_be_bytes());
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

    /// All entries this access method produces for one document: plain and
    /// single-value function indexes yield one `(key, value)` pair;
    /// multi-value function indexes yield one pair per produced value —
    /// the write side (`Collection::put`/`delete` via `index_entries`) just
    /// iterates. Each entry shares the same includes value.
    fn entry_pairs(ns_prefix: &[u8], key: &Self::Key, document: &Self::Document) -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![(Self::entry_key(ns_prefix, key, document), Self::entry_value(key, document))]
    }

    /// Scan prefix for a leftmost-prefix match over the index fields:
    /// header + the caller-side encoding of the leading index fields
    /// (e.g. `7u32.to_be_bytes()` for a u32 field; empty slice = whole
    /// index). Must not exceed the index-field segment width.
    fn entry_prefix(ns_prefix: &[u8], encoded: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ns_prefix.len() + 2 + encoded.len());
        buf.extend_from_slice(ns_prefix);
        buf.extend_from_slice(&Self::SLOT.to_be_bytes());
        buf.extend_from_slice(encoded);
        buf
    }
}

/// Leftmost-prefix scan over an index: returns each entry's decoded key
/// prefix. The key prefix is the tail segment of the entry key, so it is
/// recovered from the last `key_prefix_width()` bytes — full key when
/// `KEY_PREFIX` is empty, truncated identity otherwise (trailing fields
/// are zero-filled, use only the prefix fields).
pub fn scan_index<S: VirtualStorage, I: KvIndex>(
    store: &S,
    ns_prefix: &[u8],
    encoded: &[u8],
) -> Vec<PrefixKey<I::Key>> {
    let p = I::entry_prefix(ns_prefix, encoded);
    let taken = I::key_prefix_width();
    let kl = I::Key::KEY_LEN;
    store
        .scan_suffix(&p)
        .iter()
        .map(|suffix| {
            assert!(suffix.len() >= taken, "index entry shorter than key prefix");
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
            PrefixKey { decoded, taken }
        })
        .collect()
}
