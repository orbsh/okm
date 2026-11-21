//! A declaration compiled into the bytes an entry needs (ADR-0076 §1).
//!
//! The write path used to ask the generated `KvIndex` impls for entries, one
//! call per face, while the dynamic form ran its own runtime copy of the same
//! rules. This module holds the shared half of that: a face description is
//! compiled ONCE into field references — a key field's static offset, a hot
//! field's offset, a cold field's tag — and producing entries is then a walk
//! over those references against the key bytes and the payload bytes the write
//! already has.
//!
//! What the walk deliberately does NOT cover: a `func` face and a `where(...)`
//! predicate both read the DOCUMENT, not its bytes, so they are refused here
//! rather than guessed at. A caller that declares one keeps the generated
//! producer for that face.
//!
//! The declaration is supplied by the form that owns it — the typed side from
//! its emitted declaration table, the dynamic side from the access methods its
//! caller declared — because the store's own schema describes a face as
//! `name/slot/fields/includes/func/deprecated` and carries neither the
//! `key(...)` truncation nor the predicate (see `IndexSchema`). Compiling from
//! the store's bytes alone would write a different tail than the declaration
//! asked for.

use crate::model::entries::Entries;
use crate::model::faces::IndexDecl;
use crate::model::index::{Slot, write_slot};
use crate::model::schema::CollectionSchema;

/// One declared face, as entry production reads it.
///
/// `fields` is the sort segment in declaration order (payload fields for an
/// index, key fields for a lead face — the slot family says which), `includes`
/// is the covered segment, and `key_prefix` is the tail: the named key fields,
/// in the order the declaration lists them. An empty `key_prefix` is the whole
/// key, which is the default.
#[derive(Debug, Clone, Copy)]
pub struct FaceDecl<'a> {
    pub name: &'a str,
    pub slot: Slot,
    pub fields: &'a [&'a str],
    pub includes: &'a [&'a str],
    pub key_prefix: &'a [&'a str],
    /// `0x6nnn`: the segment is the key's own bytes (ADR-0054).
    pub lead: bool,
    /// The face carries a `func(path)`; its segment is not in the declaration bytes.
    pub func: bool,
    /// The face carries a `where(path)`; admission is not in the declaration bytes.
    pub partial: bool,
}

impl FaceDecl<'static> {
    /// The declaration table's entry as the layout compiler reads it.
    ///
    /// The slot family answers the kind, `func` / `filter` answer how the face
    /// is produced — there is no seventh field repeating any of it, which is the
    /// same rule `IndexSchema::kind` follows.
    pub fn from_index_decl(d: &'static IndexDecl) -> Self {
        Self {
            name: d.name,
            slot: d.slot,
            fields: d.fields,
            includes: d.includes,
            key_prefix: d.key_prefix,
            lead: matches!(
                crate::model::faces::FaceKind::of(d.slot),
                Some(crate::model::faces::FaceKind::Lead)
            ),
            func: !d.func.is_empty(),
            partial: !d.filter.is_empty(),
        }
    }
}

/// Why a declaration cannot be compiled into a byte walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// The face that cannot be compiled.
    pub face: String,
    pub why: &'static str,
}

impl core::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "face `{}`: {}", self.face, self.why)
    }
}

/// Why a key or payload could not be walked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Malformed {
    /// The payload is shorter than the declaration's hot segment.
    ShortPayload { hot_width: usize, got: usize },
    /// The key is shorter than the declaration reads from it.
    ShortKey { needed: usize, got: usize },
    /// A declared cold field is absent from the payload.
    MissingCold { tag: u8 },
}

impl core::fmt::Display for Malformed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ShortPayload { hot_width, got } => write!(
                f,
                "payload is {got} byte(s), shorter than the declared hot segment ({hot_width})"
            ),
            Self::ShortKey { needed, got } => {
                write!(f, "key is {got} byte(s), needs {needed}")
            }
            Self::MissingCold { tag } => write!(f, "cold field with tag {tag:#04x} is absent"),
        }
    }
}

/// A declared face compiled to field references.
#[derive(Debug, Clone)]
struct Face {
    slot: Slot,
    segment: Vec<FieldRef>,
    includes: Vec<FieldRef>,
    /// The tail's key fields, in declaration order; empty = the whole key.
    tail: Vec<FieldRef>,
}

/// Where one field's bytes sit in the key or the payload.
#[derive(Debug, Clone)]
enum FieldRef {
    /// A key field: a static slice of the key bytes.
    Key { offset: usize, width: usize },
    /// A hot payload field: a static slice of the payload's head.
    Hot { offset: usize, width: usize },
    /// A cold payload field: a TLV frame, found by tag.
    Cold { tag: u8 },
}

impl FieldRef {
    /// Append this field's bytes in the wire form an entry segment carries: the
    /// field's own encoding, no tag and no length — the order rule makes the one
    /// variable-width field the segment's last.
    ///
    /// The caller has already checked the key's length and that every cold tag
    /// is present, so a `None` here is unreachable by construction; it is
    /// answered instead of panicking because a wrong key length is caller input.
    fn append(&self, buf: &mut Vec<u8>, key: &[u8], payload: &[u8], hot_width: usize) {
        match self {
            Self::Key { offset, width } => {
                if let Some(slice) = key.get(*offset..offset + width) {
                    buf.extend_from_slice(slice);
                }
            }
            Self::Hot { offset, width } => {
                if let Some(slice) = payload.get(*offset..offset + width) {
                    buf.extend_from_slice(slice);
                }
            }
            Self::Cold { tag } => {
                if let Some(value) = cold_frame(payload, hot_width, *tag) {
                    buf.extend_from_slice(value);
                }
            }
        }
    }
}

/// The value bytes of one cold frame (`tag u8`, varint length, value), starting
/// after the declaration's hot segment.
fn cold_frame(payload: &[u8], hot_width: usize, tag: u8) -> Option<&[u8]> {
    let mut pos = hot_width;
    while pos < payload.len() {
        let t = *payload.get(pos)?;
        pos += 1;
        let (len, len_n) = crate::take_len(payload.get(pos..)?)?;
        pos += len_n;
        let end = pos.checked_add(len)?;
        let value = payload.get(pos..end)?;
        if t == tag {
            return Some(value);
        }
        pos = end;
    }
    None
}

/// One table's entry production, compiled from its declaration.
#[derive(Debug, Clone)]
pub struct EntryLayout {
    hot_width: usize,
    key_len: usize,
    faces: Vec<Face>,
}

impl EntryLayout {
    /// Compile a declaration into field references, once.
    ///
    /// Fails on the two declarations whose segment is not in the declaration
    /// bytes (`func`, `where`) and on a name no field table answers — a typo in
    /// a declaration, refused where it can still be reported.
    pub fn compile(schema: &CollectionSchema, faces: &[FaceDecl<'_>]) -> Result<Self, Unsupported> {
        let mut compiled = Vec::with_capacity(faces.len());
        for face in faces {
            if face.func {
                return Err(Unsupported {
                    face: face.name.to_string(),
                    why: "a func face is produced by its path callable, not from the declaration bytes",
                });
            }
            if face.partial {
                return Err(Unsupported {
                    face: face.name.to_string(),
                    why: "a partial face's admission is a predicate over the document, not a fact of the declaration bytes",
                });
            }
            let mut segment = Vec::with_capacity(face.fields.len());
            for name in face.fields {
                // A lead face's segment is key bytes; an index's is payload
                // bytes — the derive refuses an index field the payload does
                // not declare, so there is no second source to fall back to.
                segment.push(if face.lead {
                    key_ref(schema, name)?
                } else {
                    payload_ref(schema, name)?
                });
            }
            let mut includes = Vec::with_capacity(face.includes.len());
            for name in face.includes {
                includes.push(payload_ref(schema, name)?);
            }
            let mut tail = Vec::with_capacity(face.key_prefix.len());
            for name in face.key_prefix {
                tail.push(key_ref(schema, name)?);
            }
            compiled.push(Face {
                slot: face.slot,
                segment,
                includes,
                tail,
            });
        }
        Ok(Self {
            hot_width: schema.hot_width,
            key_len: schema.key_len,
            faces: compiled,
        })
    }

    /// Append every declared face's entries for one row.
    ///
    /// `key` is the row's encoded primary key and `payload` the bytes the write
    /// is storing (`encode_payload`). Those two are the whole input: a face whose
    /// segment comes from identity needs no payload, and one whose segment comes
    /// from the payload needs no more than the bytes already built.
    pub fn produce(
        &self,
        ns: &[u8],
        key: &[u8],
        payload: &[u8],
        out: &mut Entries,
    ) -> Result<(), Malformed> {
        if payload.len() < self.hot_width {
            return Err(Malformed::ShortPayload {
                hot_width: self.hot_width,
                got: payload.len(),
            });
        }
        if key.len() < self.key_len {
            return Err(Malformed::ShortKey {
                needed: self.key_len,
                got: key.len(),
            });
        }
        // Every cold tag the walk reads is checked before the first entry is
        // appended, so a bad payload cannot leave half a face's entries in the
        // caller's buffer.
        for face in &self.faces {
            for f in face.segment.iter().chain(face.includes.iter()) {
                if let FieldRef::Cold { tag } = f
                    && cold_frame(payload, self.hot_width, *tag).is_none()
                {
                    return Err(Malformed::MissingCold { tag: *tag });
                }
            }
        }

        for face in &self.faces {
            out.push(
                |buf| {
                    buf.extend_from_slice(ns);
                    write_slot(buf, face.slot);
                    for f in &face.segment {
                        f.append(buf, key, payload, self.hot_width);
                    }
                    if face.tail.is_empty() {
                        buf.extend_from_slice(&key[..self.key_len]);
                    } else {
                        for f in &face.tail {
                            f.append(buf, key, payload, self.hot_width);
                        }
                    }
                },
                |buf| {
                    for f in &face.includes {
                        f.append(buf, key, payload, self.hot_width);
                    }
                },
            );
        }
        Ok(())
    }

    /// How many faces this layout produces entries for.
    pub fn faces(&self) -> usize {
        self.faces.len()
    }
}

fn key_ref(schema: &CollectionSchema, name: &str) -> Result<FieldRef, Unsupported> {
    schema
        .key_fields
        .iter()
        .find(|f| f.name == name)
        .map(|f| FieldRef::Key {
            offset: f.offset,
            width: f.width,
        })
        .ok_or_else(|| Unsupported {
            face: name.to_string(),
            why: "no key field has this name",
        })
}

/// A payload field by name.
///
/// The two-source rule (a key name winning over a payload one) belongs to the
/// reduce GROUP walk, not to a face: `KvIndex::encode_named` is the payload
/// walk, and a face's `fields(...)` names payload fields. `key(...)` is how a
/// face reaches key bytes, and it only ever names the tail.
fn payload_ref(schema: &CollectionSchema, name: &str) -> Result<FieldRef, Unsupported> {
    payload_ref_opt(schema, name).ok_or_else(|| Unsupported {
        face: name.to_string(),
        why: "includes take payload fields, and no payload field has this name",
    })
}

fn payload_ref_opt(schema: &CollectionSchema, name: &str) -> Option<FieldRef> {
    if let Some(f) = schema.hot_fields.iter().find(|f| f.name == name) {
        return Some(FieldRef::Hot {
            offset: f.offset,
            width: f.width,
        });
    }
    schema
        .cold_fields
        .iter()
        .find(|f| f.name == name)
        .and_then(|f| f.tag)
        .map(|tag| FieldRef::Cold { tag })
}
