//! The structural faces a declaration owns, and the fingerprint that says so.
//!
//! A derived face (an `#[ok_index]` slot, a graph face, a path index) lives
//! outside the primary table, in its own part of the key space. Nothing in a
//! stored entry records WHICH declaration wrote it, so a store whose declaration
//! gained or moved a face cannot be told apart from one whose faces are current —
//! adding an index silently leaves it empty, and a renumbered slot silently
//! answers as its neighbour.
//!
//! The payload's `LAYOUT_VERSION` does not cover this: adding an index changes no
//! payload byte, so the layout number does not move and `ensure_layout` passes.
//!
//! So a collection declares its faces here (`INDEX_DECLS`, emitted by the derive)
//! and carries a fingerprint of them (`FACES`, computed at compile time). The
//! store is stamped with that fingerprint once (ADR-0043's meta row carries it
//! beside `version`), and [`crate::model::meta::FaceStatus`] answers whether the
//! faces on disk were built for this declaration.
//!
//! The fingerprint is COMPUTED, never written by hand: a hand-kept number is a
//! silent-error machine, because forgetting to bump it makes a stale face look
//! current. A computed one moves whenever the declaration moves.

/// One declared structural face: what `#[ok_index]` put in the key space.
///
/// `deprecated` is part of the declaration, not a comment on it: a deprecated
/// index keeps its slot (declaration order is a persistent contract) and writes
/// nothing, so its presence and its state both belong in the fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexDecl {
    pub name: &'static str,
    pub slot: u16,
    pub fields: &'static [&'static str],
    pub includes: &'static [&'static str],
    pub func: &'static str,
    pub deprecated: bool,
}

/// Bytes in a fingerprint (two independent 64-bit lanes).
/// What one [`Collection::rebuild_faces`](crate::Collection::rebuild_faces) run
/// did, in the units an operator watching a large table cares about: how many
/// entries were swept, how many rows were re-derived into how many entries, and
/// how many commits that took.
///
/// A run that fails half way answers `Err` and does NOT stamp (ADR-0048), so the
/// table keeps saying `Stale` and the next run starts over — the numbers here are
/// for the run that finished.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RebuildReport {
    /// Face namespaces emptied in the sweep phase.
    pub faces_swept: usize,
    /// Entries deleted in the sweep phase.
    pub entries_deleted: usize,
    /// Rows read and re-derived.
    pub documents: usize,
    /// Entries written by the derive phase.
    pub entries_written: usize,
    /// Commits issued across both phases.
    pub chunks: usize,
}

pub const FINGERPRINT_LEN: usize = 16;

/// The two lanes' bases differ, so the second half is not a function of the
/// first — a collision has to hit both.
const LANE_A: u64 = 0xcbf2_9ce4_8422_2325;
const LANE_B: u64 = 0x9e37_79b9_7f4a_7c15;
const PRIME: u64 = 0x0000_0100_0000_01b3;

const fn mix(h: u64, byte: u8) -> u64 {
    (h ^ byte as u64).wrapping_mul(PRIME)
}

/// Length-prefixed, so `"ab" + "c"` and `"a" + "bc"` cannot collide.
const fn mix_str(mut h: u64, s: &str) -> u64 {
    let b = s.as_bytes();
    let n = b.len() as u32;
    h = mix(h, (n >> 24) as u8);
    h = mix(h, (n >> 16) as u8);
    h = mix(h, (n >> 8) as u8);
    h = mix(h, n as u8);
    let mut i = 0;
    while i < b.len() {
        h = mix(h, b[i]);
        i += 1;
    }
    h
}

const fn mix_names(mut h: u64, names: &[&str]) -> u64 {
    h = mix(h, (names.len() >> 8) as u8);
    h = mix(h, names.len() as u8);
    let mut i = 0;
    while i < names.len() {
        h = mix_str(h, names[i]);
        i += 1;
    }
    h
}

/// A face declaration in the shape a RUNTIME declaration can build: borrowed
/// strings, no `'static` in sight.
///
/// [`IndexDecl`] is the compile-time shape (`&'static [&'static str]`) because
/// the derive emits it into a `const`. A table declared by a host language has no
/// `const` to emit into, so it describes its faces through this view and takes the
/// same bytes out of [`fingerprint_specs`]. The two agree by construction: both
/// call the one encoder below.
pub struct FaceSpec<'a> {
    pub name: &'a str,
    pub slot: u16,
    pub fields: &'a [&'a str],
    pub includes: &'a [&'a str],
    pub func: &'a str,
    pub deprecated: bool,
}

/// The per-declaration half of the fingerprint, shared by the compile-time and the
/// runtime entry points so they cannot drift apart.
const fn mix_decl(mut a: u64, mut b: u64, d: &FaceSpec<'_>) -> (u64, u64) {
    a = mix(a, (d.slot >> 8) as u8);
    a = mix(a, d.slot as u8);
    b = mix(b, (d.slot >> 8) as u8);
    b = mix(b, d.slot as u8);
    a = mix(a, d.deprecated as u8);
    b = mix(b, d.deprecated as u8);
    a = mix_str(a, d.name);
    b = mix_str(b, d.name);
    a = mix_names(a, d.fields);
    b = mix_names(b, d.fields);
    a = mix_names(a, d.includes);
    b = mix_names(b, d.includes);
    a = mix_str(a, d.func);
    b = mix_str(b, d.func);
    (a, b)
}

const fn seed() -> (u64, u64) {
    (
        mix_str(LANE_A, "okm.faces.v1"),
        mix_str(LANE_B, "okm.faces.v1"),
    )
}

const fn count(mut a: u64, mut b: u64, n: usize) -> (u64, u64) {
    a = mix(a, (n >> 8) as u8);
    a = mix(a, n as u8);
    b = mix(b, (n >> 8) as u8);
    b = mix(b, n as u8);
    (a, b)
}

const fn finish(a: u64, b: u64) -> [u8; FINGERPRINT_LEN] {
    let mut out = [0u8; FINGERPRINT_LEN];
    let mut k = 0;
    while k < 8 {
        out[k] = (a >> (56 - 8 * k)) as u8;
        out[8 + k] = (b >> (56 - 8 * k)) as u8;
        k += 1;
    }
    out
}

/// The fingerprint of a declaration's faces: canonical bytes over the
/// declarations in declaration order (order is a persistent contract — slot
/// numbering follows it, so a reordering is a different set of faces).
///
/// `const` so the number costs nothing at runtime and cannot be forgotten: it is
/// a function of the declaration, which is where it belongs.
pub const fn fingerprint(decls: &'static [IndexDecl]) -> [u8; FINGERPRINT_LEN] {
    let (mut a, mut b) = seed();
    let (na, nb) = count(a, b, decls.len());
    a = na;
    b = nb;
    let mut i = 0;
    while i < decls.len() {
        let d = decls[i];
        let spec = FaceSpec {
            name: d.name,
            slot: d.slot,
            fields: d.fields,
            includes: d.includes,
            func: d.func,
            deprecated: d.deprecated,
        };
        let (na, nb) = mix_decl(a, b, &spec);
        a = na;
        b = nb;
        i += 1;
    }
    finish(a, b)
}

/// The same bytes, from declarations a runtime built — a dynamic table naming its
/// own access methods, where `fields` / `includes` are owned strings and a `func`
/// / path may carry no name at all.
///
/// It is the same encoder, so a runtime declaration that mirrors a compile-time one
/// (same name, slot, fields, includes, func) produces the SAME number — that is what
/// makes a store built by one mode openable, and checkable, by the other.
pub fn fingerprint_specs(specs: &[FaceSpec<'_>]) -> [u8; FINGERPRINT_LEN] {
    let (mut a, mut b) = seed();
    let (na, nb) = count(a, b, specs.len());
    a = na;
    b = nb;
    for d in specs {
        let (na, nb) = mix_decl(a, b, d);
        a = na;
        b = nb;
    }
    finish(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: IndexDecl = IndexDecl {
        name: "by_name",
        slot: 0x3001,
        fields: &["name"],
        includes: &[],
        func: "",
        deprecated: false,
    };
    const B: IndexDecl = IndexDecl {
        name: "by_org",
        slot: 0x3002,
        fields: &["org_id", "created_at"],
        includes: &["bio_len"],
        func: "",
        deprecated: false,
    };

    #[test]
    fn a_declaration_has_one_fingerprint() {
        static ONE: &[IndexDecl] = &[A, B];
        static AGAIN: &[IndexDecl] = &[A, B];
        assert_eq!(fingerprint(ONE), fingerprint(AGAIN));
    }

    #[test]
    fn every_kind_of_change_moves_it() {
        static ONE: &[IndexDecl] = &[A, B];
        static REMOVED: &[IndexDecl] = &[A];
        static ADDED: &[IndexDecl] = &[
            A,
            B,
            IndexDecl {
                name: "by_hour",
                ..B
            },
        ];
        static RENAMED_FIELD: &[IndexDecl] = &[
            A,
            IndexDecl {
                fields: &["org_id", "updated_at"],
                ..B
            },
        ];
        static WIDENED: &[IndexDecl] = &[A, IndexDecl { includes: &[], ..B }];
        static DEPRECATED: &[IndexDecl] = &[
            A,
            IndexDecl {
                deprecated: true,
                ..B
            },
        ];
        static REFUNNED: &[IndexDecl] = &[
            A,
            IndexDecl {
                func: "lower_name",
                ..B
            },
        ];
        static REORDERED: &[IndexDecl] = &[B, A];
        let base = fingerprint(ONE);
        for other in [
            fingerprint(REMOVED),
            fingerprint(ADDED),
            fingerprint(RENAMED_FIELD),
            fingerprint(WIDENED),
            fingerprint(DEPRECATED),
            fingerprint(REFUNNED),
            fingerprint(REORDERED),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn a_runtime_declaration_and_a_compile_time_one_agree() {
        // The whole point of `FaceSpec`: a host-declared table must be able to
        // produce the number the typed path would, so a store crosses between the
        // two modes without a false `Stale`.
        let spec = FaceSpec {
            name: A.name,
            slot: A.slot,
            fields: A.fields,
            includes: A.includes,
            func: A.func,
            deprecated: A.deprecated,
        };
        assert_eq!(fingerprint(&[A]), fingerprint_specs(&[spec]));
        assert_eq!(fingerprint(&[]), fingerprint_specs(&[]));
    }

    #[test]
    fn a_split_string_is_not_the_joined_one() {
        // Length prefixes are why this is not a real hazard; the test is the lock.
        static SPLIT: &[IndexDecl] = &[A, IndexDecl { name: "b", ..A }];
        static JOINED: &[IndexDecl] = &[IndexDecl { name: "ab", ..A }, IndexDecl { name: "", ..A }];
        assert_ne!(fingerprint(SPLIT), fingerprint(JOINED));
    }
}
