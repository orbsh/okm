//! Table meta — fixed face 0x1F: ONE entry per table that describes the store
//! instead of holding its data (ADR-0043).
//!
//! The address is a face-level singleton, `[partition][ns][0x1F]` with no
//! suffix, exactly like the vocabulary cursor at `[ns][0x02]`. 0x1F is the
//! HIGHEST fixed face, and that is deliberate: `slot <= 0x1F` is the delimiter
//! that separates the one-byte faces from the counting families, so the meta
//! sits on the boundary and no future fixed face can squeeze it out.
//!
//! The value is a NAME → BYTES map, not a fixed struct. Three names are the
//! runtime's: `version` (the layout generation this build speaks, one byte),
//! `faces` (the fingerprint of the derived faces, ADR-0048) and, when the
//! declaration carries one, `table`. Anything else a consumer wants to record
//! about the store (a build id, a migration note) rides beside them without a
//! layout decision. Names are inline — the meta row never touches the field-name
//! vocabulary (slots 0x02/0x03), so stamping a store cannot grow a vocabulary or
//! depend on one.
//!
//! `version` is load-bearing, not decorative. A layout change is a cutover: the
//! number increments, the store is exported by the build that wrote it and
//! imported into one the new build creates, and an older store is REFUSED rather
//! than half-read (ADR-0030, `docs/internals/key-layout.md`). So the check is
//! total in both directions: the first-touch verb stamps the number on a row it
//! creates, and a row that exists without one is a store this build did not
//! stamp — refused with [`crate::Error::LayoutUnstamped`], exactly as a number
//! this build does not speak is refused with [`crate::Error::LayoutMismatch`].
//! There is no third state to tolerate: nothing here is migrated, so a row
//! written by an older layout is not this build's store.
//!
//! What it is NOT: a compatibility promise (ADR-0042). It turns "this store was
//! written in another shape" into one point read at attach time instead of a
//! decode failure on the first row; it does not read, migrate or reinterpret a
//! single stored payload. Migration is export (the build that wrote the rows) →
//! import (the build that reads them).

use std::collections::BTreeMap;

use crate::engine::storage::VirtualStorage;
use crate::error::Result;
use crate::model::index::{Document, META_SLOT, write_slot};
use crate::model::wrappers::wire::{put_len, take_len};

/// The layout generation this build speaks (`Document::LAYOUT_VERSION`), one
/// byte — the same number the declaration names in `#[ok_layout(version = N)]`.
///
/// This is the store's answer to "which key layout wrote me", and it is what
/// makes a refusal possible at attach: a layout change is a cutover, so a store
/// of another generation is turned away by NUMBER rather than half-read row by
/// row. The name is therefore required, not optional: a row that carries other
/// names but not this one is a store this build did not stamp, and it is refused
/// ([`crate::Error::LayoutUnstamped`]) instead of being opened on the assumption
/// that silence means "older". Only an ABSENT ROW is exempt — there is nothing to
/// disagree with, and `ensure_layout` seeds it.
pub const VERSION: &str = "version";

/// The fingerprint of the declaration's structural faces (ADR-0048) — the
/// derived state that lives outside the primary table and that the payload's
/// layout number cannot describe: adding an index changes no payload byte, so
/// the layout does not move while the faces do.
///
/// Absent means "not described" (every store written before this existed), which
/// reads as untrusted — never as "this table has no faces".
pub const FACES: &str = "faces";

/// The table's own name (`#[ok_name("...")]`), recorded at first touch so the
/// store can answer "is this ns already somebody's" without seeing another
/// module's declarations.
///
/// This is the cross-module half of the ns claim. The derive catches a duplicate
/// ns inside one module at compile time (`E0428`, a name per ns); a table declared
/// in another module or another crate is invisible to it, but not to the store:
/// two tables in one ns share this row, so the second name to arrive is a
/// conflict rather than a silent second tenant.
///
/// Absent means "the store predates or never recorded a name", which is not a
/// conflict — an unnamed declaration writes nothing and checks nothing.
pub const TABLE: &str = "table";

/// What a store's face stamp says about this build's declaration (ADR-0048).
///
/// Three answers, not two, because they mean different things to a caller: a
/// stale face is repairable drift, while a store that never recorded one is
/// simply older than the mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaceStatus {
    /// The store's faces were built from this declaration.
    Match,
    /// The store carries a stamp from another declaration: its derived faces
    /// answer for a key layout this build does not use.
    Stale {
        stored: [u8; crate::model::faces::FINGERPRINT_LEN],
        current: [u8; crate::model::faces::FINGERPRINT_LEN],
    },
    /// No stamp — the store predates the mechanism (or was never stamped at
    /// attach). Treat its faces as untrusted; nothing here says they are wrong,
    /// only that nothing says they are right.
    Unknown {
        current: [u8; crate::model::faces::FINGERPRINT_LEN],
    },
}

impl FaceStatus {
    /// Whether a caller may trust the derived faces without a rebuild.
    pub fn is_current(&self) -> bool {
        matches!(self, FaceStatus::Match)
    }
}

/// The meta entry of one table: a name → value map with deterministic order
/// (`BTreeMap`, so the encoded bytes are stable and hex-lockable).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableMeta {
    entries: BTreeMap<String, Vec<u8>>,
}

impl TableMeta {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries.get(name).map(|v| v.as_slice())
    }

    pub fn set(&mut self, name: &str, value: Vec<u8>) {
        self.entries.insert(name.to_string(), value);
    }

    pub fn remove(&mut self, name: &str) -> Option<Vec<u8>> {
        self.entries.remove(name)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// The table name this store records, if any (`#[ok_name]`).
    pub fn table_name(&self) -> Option<&str> {
        self.get(TABLE)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    }

    /// The stamped face fingerprint, if this row names one. A value of the wrong
    /// width is `None`: this row is written by this crate, so a short one is a
    /// foreign write, not a fingerprint to compare half of.
    pub fn faces(&self) -> Option<[u8; crate::model::faces::FINGERPRINT_LEN]> {
        let bytes = self.get(FACES)?;
        <[u8; crate::model::faces::FINGERPRINT_LEN]>::try_from(bytes).ok()
    }

    /// The stamped layout generation, if this row names one. Exactly one byte:
    /// this row is written by this crate, so a wider or empty value is a foreign
    /// write, not a number to compare half of — it reads as absent, the same way
    /// a short `faces` value does.
    pub fn layout_version(&self) -> Option<u8> {
        match self.get(VERSION)? {
            [byte] => Some(*byte),
            _ => None,
        }
    }

    /// This store's faces against a declaration's, as three answers.
    pub fn face_status(&self, current: [u8; crate::model::faces::FINGERPRINT_LEN]) -> FaceStatus {
        match self.faces() {
            Some(stored) if stored == current => FaceStatus::Match,
            Some(stored) => FaceStatus::Stale { stored, current },
            None => FaceStatus::Unknown { current },
        }
    }

    /// `[count u16 BE]` then once per entry `[name_len varint][name bytes]
    /// [value_len varint][value bytes]`, in name order.
    ///
    /// The count is the frame's own bound: a truncated or over-long buffer has
    /// no silent reading, and the varint lengths are the SAME codec every other
    /// TLV frame in okm uses (`wire::put_len`).
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + self.entries.len() * 16);
        buf.extend_from_slice(&(self.entries.len() as u16).to_be_bytes());
        for (name, value) in &self.entries {
            put_len(&mut buf, name.len());
            buf.extend_from_slice(name.as_bytes());
            put_len(&mut buf, value.len());
            buf.extend_from_slice(value);
        }
        buf
    }

    /// Decode the meta payload. Aborts on bytes this SDK cannot have written
    /// (ADR-0040's codec rule): a truncated frame, invalid UTF-8 in a name, a
    /// duplicate name, or trailing bytes.
    pub fn decode(bytes: &[u8]) -> Self {
        assert!(
            bytes.len() >= 2,
            "table meta: payload shorter than its entry count"
        );
        let count = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
        let mut off = 2;
        let mut entries = BTreeMap::new();
        for _ in 0..count {
            let (name_len, n) = take_len(&bytes[off..]).expect("table meta: truncated name length");
            off += n;
            assert!(
                bytes.len() >= off + name_len,
                "table meta: truncated name ({} bytes claimed, {} left)",
                name_len,
                bytes.len() - off
            );
            let name = String::from_utf8(bytes[off..off + name_len].to_vec())
                .expect("table meta: name is not UTF-8");
            off += name_len;
            let (value_len, n) =
                take_len(&bytes[off..]).expect("table meta: truncated value length");
            off += n;
            assert!(
                bytes.len() >= off + value_len,
                "table meta: truncated value ({} bytes claimed, {} left)",
                value_len,
                bytes.len() - off
            );
            let value = bytes[off..off + value_len].to_vec();
            off += value_len;
            assert!(
                entries.insert(name.clone(), value).is_none(),
                "table meta: duplicate name `{name}`"
            );
        }
        assert_eq!(
            off,
            bytes.len(),
            "table meta: {} trailing bytes after {count} entries",
            bytes.len() - off
        );
        Self { entries }
    }
}

/// The meta entry's key: the table header with the highest fixed face. No
/// suffix — the face is a singleton, so `[ns][0x1F]` is the whole address.
pub fn meta_key(header: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(header.len() + 1);
    buf.extend_from_slice(header);
    write_slot(&mut buf, META_SLOT);
    buf
}

/// Read a table's meta entry. `None` = never stamped (a store this build did
/// not create); engine failures come back as `Err`.
pub fn read<S: VirtualStorage + ?Sized>(store: &S, header: &[u8]) -> Result<Option<TableMeta>> {
    let key = meta_key(header);
    Ok(store.get(&key)?.map(|bytes| TableMeta::decode(&bytes)))
}

/// Write the meta row as given, replacing whatever it carried.
///
/// This is the raw write and it does not touch `version`: `Collection::set_meta`
/// (and the dynamic twin) reconciles the generation before calling it, so the
/// entry cannot be edited away by accident. A test that wants a FOREIGN row —
/// one this build did not write — comes through here.
pub fn write<S: VirtualStorage + ?Sized>(store: &S, header: &[u8], meta: &TableMeta) -> Result<()> {
    store.put(meta_key(header), meta.encode())
}

/// Refuse a row that does not record THIS build's layout generation.
///
/// The one comparison the row exists for, and it has no permissive branch: a row
/// that names another number is [`crate::Error::LayoutMismatch`] (naming both), and
/// a row that names none is [`crate::Error::LayoutUnstamped`]. Both are the same
/// verdict — this store is not of this build's layout — because a layout change is
/// a cutover and nothing here migrates (ADR-0043 §4). "No name" is not
/// older-but-fine: a row that exists was written by something, and this build did
/// not write it. Only a store with NO ROW is absent, and that is the callers' seed
/// path, not a comparison.
pub fn check_layout(meta: &TableMeta, layout_version: u8) -> Result<()> {
    match meta.layout_version() {
        Some(stored) if stored != layout_version => Err(crate::error::Error::LayoutMismatch {
            stored,
            current: layout_version,
        }),
        Some(_) => Ok(()),
        None => Err(crate::error::Error::LayoutUnstamped {
            current: layout_version,
        }),
    }
}

/// First-touch: stamp a FRESH table's meta row with the layout generation and the
/// declaration's face fingerprint; on an existing row, refuse a row that is not of
/// this generation and otherwise answer it exactly as it stands.
///
/// "Not of this generation" covers both shapes of an existing row: a `version`
/// this build does not speak is [`crate::Error::LayoutMismatch`] naming both
/// numbers, and a row that records no `version` at all is
/// [`crate::Error::LayoutUnstamped`]. A fresh store has no row to compare, so it is
/// seeded and never refused — the refusal path exists for a row that is there and
/// does not match.
///
/// "Exactly as it stands" is the point for `faces`. A row that exists without a
/// fingerprint is a store written before faces existed, or one whose rebuild never
/// finished — both answer `Stale`/`Unknown` by design (ADR-0048), and a first-touch
/// verb that repaired the row would erase that signal. Only [`stamp_layout`] moves a
/// fingerprint on a store that already has a row, and only the rebuild calls it.
///
/// `version` is different, and the difference is the whole reason it is written:
/// a fingerprint is repairable drift, a generation is a cutover. So this verb
/// compares it, and a disagreement is a refusal — the store is not half-read, it is
/// refused, at the one place a reader already says "I am attaching to this store".
pub fn ensure_layout<S: VirtualStorage + ?Sized>(
    store: &S,
    header: &[u8],
    layout_version: u8,
    faces: [u8; crate::model::faces::FINGERPRINT_LEN],
) -> Result<TableMeta> {
    let key = meta_key(header);
    let Some(bytes) = store.get(&key)? else {
        let mut meta = TableMeta::new();
        meta.set(VERSION, vec![layout_version]);
        meta.set(FACES, faces.to_vec());
        store.put(key, meta.encode())?;
        return Ok(meta);
    };
    let meta = TableMeta::decode(&bytes);
    check_layout(&meta, layout_version)?;
    Ok(meta)
}

/// Write both of the runtime's layout names, creating the meta row if the store
/// has none.
///
/// This is the ONE verb that moves `faces` after a store already exists — the
/// rebuild calls it on completion, which is why an interrupted rebuild needs no
/// state of its own: the stamp simply was never written, so the next open still
/// answers `Stale` (ADR-0048). It carries `version` too, so a rebuild on a store
/// that has no meta row produces the same row a first touch would. A row that IS
/// there is checked first: another generation's number, or no number at all, is
/// refused (`LayoutMismatch` / `LayoutUnstamped`) rather than relabelled — the
/// store's own number is a statement, and this verb may not make one up for rows it
/// did not write.
pub fn stamp_layout<S: VirtualStorage + ?Sized>(
    store: &S,
    header: &[u8],
    layout_version: u8,
    faces: [u8; crate::model::faces::FINGERPRINT_LEN],
) -> Result<TableMeta> {
    let key = meta_key(header);
    let mut meta = match store.get(&key)? {
        Some(bytes) => {
            let meta = TableMeta::decode(&bytes);
            check_layout(&meta, layout_version)?;
            meta
        }
        None => TableMeta::new(),
    };
    meta.set(VERSION, vec![layout_version]);
    meta.set(FACES, faces.to_vec());
    store.put(key, meta.encode())?;
    Ok(meta)
}

/// The table header a `Document` writes under: `[partition?][ns]`.
pub fn document_header<R: Document>() -> Vec<u8> {
    let mut header = Vec::with_capacity(3);
    header.extend_from_slice(R::PARTITION_PREFIX);
    header.extend_from_slice(R::NS_PREFIX);
    header
}
