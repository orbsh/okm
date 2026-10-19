//! Field paths over the schema-free zone (ADR-0029).
//!
//! A path addresses one location in a document: `author.name`,
//! `refs[0].href`, `tags`. It exists because the dynamic segment is
//! nested — an index over a dynamic field needs to name a location, not
//! a top-level column.
//!
//! Two component kinds are two NAMESPACES, not two spellings of one
//! thing:
//!
//! - [`Component::Field`] / [`Component::Key`] address a **name** — in
//!   the canonical byte form the name is a handle from the table's
//!   field-name vocabulary (ADR-0012), i.e. a stable identity;
//! - [`Component::Index`] addresses a **position** in an array — data,
//!   never interned, and not stable across mutations (an insert shifts
//!   every later position).
//!
//! The walker's next action differs for the two (`map` lookup vs array
//! subscript), which is why the distinction is load-bearing rather than
//! cosmetic. Collapsing them into one integer would niche the id space
//! behind an implicit convention — the same "the declaration would lie"
//! failure ADR-0001's direction-bit proposal was rejected for.
//!
//! Canonical byte form (self-delimiting, ONE tag byte per component,
//! **no leading component count**):
//!
//! ```text
//! Field → [0x01][handle]        handle: u16 in the compact lane
//! Index → [0x02][u32 BE]
//! Key   → [0x03][len][bytes]    len in the compact lane
//! ```
//!
//! No count, deliberately: with a count first, comparison would sort by
//! DEPTH before the path, so a subtree's entries would no longer be
//! adjacent and `[a]` would stop being a byte prefix of `[a][0][b]`.
//! Without it a path is a byte prefix of every extension of itself, so
//! "scan everything under this path" stays one prefix scan, array
//! wildcards fall out for free, and depth is derivable by walking.
//!
//! The entry key of a path index does NOT carry the path (the slot is
//! the path's identity); this encoding is the form paths take where they
//! are DATA — a declaration carrier or an exported schema.

use crate::Value;

/// One path component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Component {
    /// A field name (object key): the interned-name arm.
    Field(String),
    /// An array position (a pinned element).
    Index(u32),
    /// An object key that is not interned: arbitrary bytes, carried
    /// literally.
    Key(Vec<u8>),
}

const TAG_FIELD: u8 = 0x01;
const TAG_INDEX: u8 = 0x02;
const TAG_KEY: u8 = 0x03;

/// The compact lane shared with the field-id encoding (ADR-0012):
/// values `< 0xFF` ride one byte, `0xFF` escapes to a u16 BE tail, so
/// growth never requires a migration. Prefix-monotone (ADR-0020's
/// VarInt property), which is what keeps byte order meaningful.
fn put_compact(buf: &mut Vec<u8>, v: u16) {
    if v < 0xFF {
        buf.push(v as u8);
    } else {
        buf.push(0xFF);
        buf.extend_from_slice(&v.to_be_bytes());
    }
}

/// Consume a compact-lane value; returns the bytes consumed.
fn take_compact(bytes: &[u8]) -> Result<(u16, usize), String> {
    let first = *bytes.first().ok_or_else(truncated)?;
    if first < 0xFF {
        return Ok((first as u16, 1));
    }
    if bytes.len() < 3 {
        return Err(truncated());
    }
    Ok((u16::from_be_bytes([bytes[1], bytes[2]]), 3))
}

fn truncated() -> String {
    "path encoding truncated".to_string()
}

/// A path over one document.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Path {
    components: Vec<Component>,
}

impl Path {
    /// A path from its components (empty = the document itself, which
    /// resolves to nothing: a document is not a value).
    pub fn new(components: Vec<Component>) -> Self {
        Self { components }
    }

    /// Parse the compact text form: dot-separated segments, `name`,
    /// `name[i]`, `[i]`; a segment wrapped in double quotes is a
    /// literal (non-interned) key, so `"a.b"` is one key rather than
    /// two components.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Ok(Self::default());
        }
        let chars: Vec<char> = text.chars().collect();
        let mut components = Vec::new();
        let mut at_component_start = true;
        let mut i = 0usize;
        while i < chars.len() {
            match chars[i] {
                '.' => {
                    if at_component_start {
                        return Err(format!("empty path segment in `{text}`"));
                    }
                    at_component_start = true;
                    i += 1;
                }
                '"' => {
                    if !at_component_start {
                        return Err(format!("expected `.` or `[` before `\"` in `{text}`"));
                    }
                    let start = i + 1;
                    let mut j = start;
                    while j < chars.len() && chars[j] != '"' {
                        j += 1;
                    }
                    if j >= chars.len() {
                        return Err(format!("unclosed literal key in `{text}`"));
                    }
                    let key: String = chars[start..j].iter().collect();
                    components.push(Component::Key(key.into_bytes()));
                    at_component_start = false;
                    i = j + 1;
                }
                '[' => {
                    let start = i + 1;
                    let mut j = start;
                    while j < chars.len() && chars[j] != ']' {
                        j += 1;
                    }
                    if j >= chars.len() {
                        return Err(format!("unclosed index in `{text}`"));
                    }
                    let inner: String = chars[start..j].iter().collect();
                    let idx: u32 = inner
                        .parse()
                        .map_err(|_| format!("bad array index `{inner}` in `{text}`"))?;
                    components.push(Component::Index(idx));
                    at_component_start = false;
                    i = j + 1;
                }
                _ => {
                    if !at_component_start {
                        return Err(format!("expected `.` or `[` after a component in `{text}`"));
                    }
                    let start = i;
                    while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                        i += 1;
                    }
                    let name: String = chars[start..i].iter().collect();
                    if name.is_empty() {
                        return Err(format!("empty path segment in `{text}`"));
                    }
                    components.push(Component::Field(name));
                    at_component_start = false;
                }
            }
        }
        if at_component_start {
            return Err(format!("empty path segment in `{text}`"));
        }
        Ok(Self { components })
    }

    pub fn components(&self) -> &[Component] {
        &self.components
    }

    /// Number of components (the path's depth).
    pub fn depth(&self) -> usize {
        self.components.len()
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    /// Resolve the path over a document map. Fan-out semantics:
    ///
    /// - an array with a following `Index` component addresses exactly
    ///   that element; **any other component after an array fans out
    ///   over every element** (wildcard = the ABSENCE of `Index`, so no
    ///   third component kind is needed);
    /// - a path that ends at an array fans out over its scalar elements
    ///   (the multi-value index regime);
    /// - a missing name, an out-of-range index, or a path that ends at
    ///   an object resolves to NO values — never an error (the same
    ///   "absent attribute values are empty results" rule the attribute
    ///   faces carry).
    pub fn resolve<'a>(&self, document: &'a crate::ValueMap) -> Vec<&'a Value> {
        let mut out = Vec::new();
        walk_map(document, &self.components, &mut out);
        out
    }

    /// The canonical byte form. `resolve_name` interns a name through
    /// the table's field-name vocabulary (allocate on first sight on
    /// the write path; a plain lookup on the read path).
    pub fn encode(&self, mut resolve_name: impl FnMut(&str) -> u16) -> Vec<u8> {
        let mut buf = Vec::new();
        for c in &self.components {
            match c {
                Component::Field(name) => {
                    buf.push(TAG_FIELD);
                    put_compact(&mut buf, resolve_name(name));
                }
                Component::Index(i) => {
                    buf.push(TAG_INDEX);
                    buf.extend_from_slice(&i.to_be_bytes());
                }
                Component::Key(k) => {
                    buf.push(TAG_KEY);
                    let len = u16::try_from(k.len()).unwrap_or(u16::MAX);
                    put_compact(&mut buf, len);
                    buf.extend_from_slice(&k[..k.len().min(u16::MAX as usize)]);
                }
            }
        }
        buf
    }

    /// Inverse of [`Path::encode`]. `name_of` resolves a vocabulary
    /// handle back to its name; an unknown handle is an error (a path
    /// can only be decoded against the table it was written for — the
    /// vocabulary is per table, so a path is table-scoped by
    /// construction).
    pub fn decode(bytes: &[u8], mut name_of: impl FnMut(u16) -> Option<String>) -> Result<Self, String> {
        let mut components = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            let tag = bytes[i];
            i += 1;
            match tag {
                TAG_FIELD => {
                    let (id, used) = take_compact(&bytes[i..])?;
                    i += used;
                    let name = name_of(id).ok_or_else(|| format!("unknown field handle {id}"))?;
                    components.push(Component::Field(name));
                }
                TAG_INDEX => {
                    if bytes.len() - i < 4 {
                        return Err(truncated());
                    }
                    let idx = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
                    i += 4;
                    components.push(Component::Index(idx));
                }
                TAG_KEY => {
                    let (len, used) = take_compact(&bytes[i..])?;
                    i += used;
                    let len = len as usize;
                    if bytes.len() - i < len {
                        return Err(truncated());
                    }
                    components.push(Component::Key(bytes[i..i + len].to_vec()));
                    i += len;
                }
                other => return Err(format!("unknown path component tag {other}")),
            }
        }
        Ok(Self { components })
    }
}

fn walk_map<'a>(map: &'a crate::ValueMap, comps: &[Component], out: &mut Vec<&'a Value>) {
    let Some((head, rest)) = comps.split_first() else {
        return; // a document is not a value
    };
    let name: Option<&str> = match head {
        Component::Field(n) => Some(n.as_str()),
        Component::Key(k) => std::str::from_utf8(k).ok(),
        Component::Index(_) => None, // an array position on a map: nothing
    };
    if let Some(n) = name
        && let Some(v) = map.get(n)
    {
        walk_value(v, rest, out);
    }
}

fn walk_value<'a>(v: &'a Value, comps: &[Component], out: &mut Vec<&'a Value>) {
    match v {
        Value::Obj(m) => walk_map(m, comps, out),
        Value::Array(items) => {
            if comps.is_empty() {
                // A path ending at an array fans out over its scalar
                // leaves (the multi-value index regime).
                out.extend(flatten_scalars(v));
                return;
            }
            match comps.first() {
                // Pinned element.
                Some(Component::Index(i)) => {
                    if let Some(x) = items.get(*i as usize) {
                        walk_value(x, &comps[1..], out);
                    }
                }
                // Wildcard: fan out over every element with the SAME
                // remaining components.
                _ => {
                    for x in items {
                        walk_value(x, comps, out);
                    }
                }
            }
        }
        // A non-composite: only a path that ENDS here yields a value.
        _ => {
            if comps.is_empty() {
                out.push(v);
            }
        }
    }
}

/// Scalar leaves of a (possibly nested) array, in order.
fn flatten_scalars(v: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    fn go<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
        match v {
            Value::Array(items) => {
                for x in items {
                    go(x, out);
                }
            }
            Value::Obj(_) => {}
            other => out.push(other),
        }
    }
    go(v, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn obj(pairs: &[(&str, Value)]) -> Value {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        Value::Obj(m)
    }

    fn doc(pairs: &[(&str, Value)]) -> crate::ValueMap {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        m
    }

    #[test]
    fn parse_forms() {
        assert_eq!(
            Path::parse("author.name").unwrap(),
            Path::new(vec![
                Component::Field("author".into()),
                Component::Field("name".into())
            ])
        );
        assert_eq!(
            Path::parse("refs[3].href").unwrap(),
            Path::new(vec![
                Component::Field("refs".into()),
                Component::Index(3),
                Component::Field("href".into())
            ])
        );
        assert_eq!(
            Path::parse("[1]").unwrap(),
            Path::new(vec![Component::Index(1)])
        );
        // A quoted segment is one literal key, dots and all.
        assert_eq!(
            Path::parse("\"a.b\"").unwrap(),
            Path::new(vec![Component::Key(b"a.b".to_vec())])
        );
        assert!(Path::parse("a..b").is_err());
        assert!(Path::parse("a[x]").is_err());
        assert!(Path::parse("a[1").is_err());
    }

    #[test]
    fn resolve_nested_wildcard_and_pinned() {
        let d = doc(&[
            (
                "author",
                obj(&[("name", Value::Str("bob".into()))]),
            ),
            (
                "tags",
                Value::Array(vec![Value::Str("x".into()), Value::Str("y".into())]),
            ),
            (
                "refs",
                Value::Array(vec![
                    obj(&[("href", Value::Str("first".into()))]),
                    obj(&[("href", Value::Str("second".into()))]),
                ]),
            ),
        ]);

        // Nested field.
        let hits = Path::parse("author.name").unwrap().resolve(&d);
        assert_eq!(hits, vec![&Value::Str("bob".into())]);

        // A path ending at an array fans out over its scalar leaves.
        let tags = Path::parse("tags").unwrap().resolve(&d);
        assert_eq!(tags.len(), 2);

        // Array without an Index component: wildcard fan-out.
        let hrefs = Path::parse("refs.href").unwrap().resolve(&d);
        assert_eq!(hrefs, vec![&Value::Str("first".into()), &Value::Str("second".into())]);

        // Pinned element: exactly one.
        let pinned = Path::parse("refs[1].href").unwrap().resolve(&d);
        assert_eq!(pinned, vec![&Value::Str("second".into())]);

        // Out of range / absent / ending at an object: no values, no error.
        assert!(Path::parse("refs[9].href").unwrap().resolve(&d).is_empty());
        assert!(Path::parse("nope.name").unwrap().resolve(&d).is_empty());
        assert!(Path::parse("author").unwrap().resolve(&d).is_empty());
        // A scalar with components left over: nothing.
        assert!(Path::parse("tags.more").unwrap().resolve(&d).is_empty());
    }

    #[test]
    fn canonical_encoding_has_no_leading_count_and_stays_prefix_shareable() {
        // The same fake vocabulary for both paths: "a" -> 5, "b" -> 9.
        let vocab = |n: &str| match n {
            "a" => 5u16,
            "b" => 9u16,
            _ => 200u16,
        };
        let short = Path::parse("a").unwrap().encode(vocab);
        let long = Path::parse("a.b").unwrap().encode(vocab);
        assert_eq!(short, vec![TAG_FIELD, 5]);
        // No count in front: the shorter path is a byte PREFIX of the longer.
        assert_eq!(&long[..short.len()], &short[..]);
        assert_eq!(long, vec![TAG_FIELD, 5, TAG_FIELD, 9]);

        // Array wildcard stays a prefix relationship too: `a` is a prefix
        // of `a[0].b` because the index component is just appended.
        let indexed = Path::parse("a[0].b").unwrap().encode(vocab);
        assert_eq!(&indexed[..short.len()], &short[..]);
        assert_eq!(indexed, vec![TAG_FIELD, 5, TAG_INDEX, 0, 0, 0, 0, TAG_FIELD, 9]);

        // Compact lane boundary: 0xFE stays one byte, 0xFF escapes.
        let lane = Path::parse("a").unwrap().encode(|_| 0xFE);
        assert_eq!(lane, vec![TAG_FIELD, 0xFE]);
        let escaped = Path::parse("a").unwrap().encode(|_| 0x1234);
        assert_eq!(escaped, vec![TAG_FIELD, 0xFF, 0x12, 0x34]);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let p = Path::parse("refs[2].\"a.b\"").unwrap();
        let names: BTreeMap<u16, String> = [(0u16, "refs".to_string()), (1u16, "a.b".to_string())]
            .into_iter()
            .collect();
        let bytes = p.encode(|n| match n {
            "refs" => 0,
            "a.b" => 1,
            other => panic!("unexpected name {other}"),
        });
        let back = Path::decode(&bytes, |id| names.get(&id).cloned()).unwrap();
        assert_eq!(back, p);
        // Unknown handle / unknown tag / truncation are errors.
        assert!(Path::decode(&[TAG_FIELD, 7], |_| None).is_err());
        assert!(Path::decode(&[0x7F], |_| None).is_err());
        assert!(Path::decode(&[TAG_INDEX, 0], |_| None).is_err());
    }
}