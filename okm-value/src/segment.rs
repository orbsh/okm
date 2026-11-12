//! The dynamic segment's block-table layout (ADR-0057). Slot 1 holds ONE entry
//! per obj; its value is:
//!
//! ```text
//! value  = toc_len u16 BE | blocks | values
//! values = leaf bodies, block by block: block 0's leaves, then block 1's, …
//! blocks = block*            (block 0 = the top level; children breadth-first)
//! block  = count varint | entry*
//! entry  = id varint | head u8 | body
//! head   = bit 7      composite: this entry owns the next block
//!        | bits 6..4  len: 0..6 inline, 7 = escape (a `put_len` varint follows)
//!        | bits 3..0  value type (0..8)
//! ```
//!
//! Two properties earn the layout its keep. The table is contiguous, so a
//! reader walks entries without touching the value bytes in between — the case
//! that hurts is many fields, or one large `Bytes` among small ones, where the
//! old inline frames scattered every header across the value span. And no
//! offsets are stored: a block's base is the sum of the leaf lengths before it,
//! which is a few additions over bytes already in hand.
//!
//! The *j*-th composite entry met while walking the blocks owns the *j+1*-th
//! block — that is what makes a stored offset unnecessary for nesting, and it
//! makes the writer's queue order part of the format.
//!
//! Ids are the field-name vocabulary's numbers; `0` means "no name", which is
//! what an array element carries. Bit 7 of the type byte marks an entry that
//! owns a block, so every entry is self-delimiting: a reader knows whether a
//! `len` follows without understanding the type, and an unknown type stays
//! skippable ("a newer writer stored something I cannot see") instead of
//! refusing the segment.

use crate::Error;
use crate::value::ObjValueType;
use crate::value::{DynamicField, ValueMap, Variant};
use crate::varint::{put_len, take_len};
use std::collections::BTreeMap;

/// Bit 7 of an entry's head: this entry owns the next block.
const COMPOSITE: u8 = 0x80;
/// Bits 6..4 of the head: the body's length while it is 6 or less.
const LEN_SHIFT: u8 = 4;
/// The length code that means "a `put_len` varint follows".
const LEN_ESCAPE: u8 = 0x07;
/// The low nibble names the value type.
const TYPE_MASK: u8 = 0x0F;

/// The three types whose body length is decided by the type rather than by the
/// head's length field — writing it down would repeat what the type already says.
fn implied_len(ty: u8) -> Option<usize> {
    match ObjValueType::from_byte(ty)? {
        ObjValueType::F64 => Some(8),
        ObjValueType::Bool => Some(1),
        ObjValueType::Null => Some(0),
        _ => None,
    }
}
/// The head: `toc_len u16 BE`, reserved and patched before the value is stored.
const HEAD: usize = 2;
/// Deepest nesting a reader will follow. A nested value costs a frame on the
/// stack, and the segment is input: refuse rather than overflow.
const MAX_DEPTH: usize = 64;

/// One entry of a block, as the reader sees it: a leaf points into the value
/// area, a composite names the block it owns.
struct Item {
    id: u16,
    ty: u8,
    composite: bool,
    off: usize,
    len: usize,
    child: usize,
}

/// One entry of a block, in the writer's hands.
struct Entry {
    id: u16,
    ty: u8,
    len: usize,
    composite: bool,
}

/// Encode a name-keyed map as one segment value.
///
/// Names resolve (and allocate) through `vocab`; a nested object's names go
/// through the same vocabulary — one vocabulary per table, so nesting needs no
/// second one.
pub fn encode<E>(
    fields: &ValueMap,
    vocab: &mut dyn FnMut(&str) -> Result<u16, E>,
) -> Result<Vec<u8>, Error<E>> {
    let mut values: Vec<u8> = Vec::new();
    let mut blocks: Vec<Vec<Entry>> = vec![Vec::new()];
    // Composite values in emit order. The j-th one owns block j+1, which is the
    // whole of the nesting contract.
    let mut pending: Vec<&Variant> = Vec::new();

    for (name, value) in fields {
        let id = vocab(name)?;
        push(&mut values, &mut blocks[0], &mut pending, id, value);
    }
    let mut qi = 0;
    while qi < pending.len() {
        let value = pending[qi];
        qi += 1;
        let mut block: Vec<Entry> = Vec::new();
        match value {
            Variant::Map(map) => {
                for (name, child) in map {
                    let id = vocab(name)?;
                    push(&mut values, &mut block, &mut pending, id, child);
                }
            }
            Variant::Array(items) => {
                for child in items {
                    push(&mut values, &mut block, &mut pending, 0, child);
                }
            }
            _ => unreachable!("only composites are queued"),
        }
        blocks.push(block);
    }

    let mut out: Vec<u8> = Vec::with_capacity(HEAD + 64 + values.len());
    out.extend_from_slice(&[0, 0]);
    for block in &blocks {
        put_len(&mut out, block.len());
        for entry in block {
            write_id(&mut out, entry.id);
            // One head byte: composite flag, length code, type nibble.
            let (head, escaped) = if entry.composite {
                (COMPOSITE | entry.ty, false)
            } else if implied_len(entry.ty).is_some() {
                (entry.ty, false)
            } else if entry.len <= 6 {
                ((entry.len as u8) << LEN_SHIFT | entry.ty, false)
            } else {
                (LEN_ESCAPE << LEN_SHIFT | entry.ty, true)
            };
            out.push(head);
            if escaped {
                put_len(&mut out, entry.len);
            }
        }
    }
    let toc_len = out.len() - HEAD;
    let toc_len = u16::try_from(toc_len).map_err(|_| Error::Corrupt {
        what: format!("dynamic segment table is {toc_len} bytes; the head holds 16 bits"),
    })?;
    out[..HEAD].copy_from_slice(&toc_len.to_be_bytes());
    out.extend_from_slice(&values);
    Ok(out)
}

/// One entry into `block`: a scalar writes its body into the value area, a
/// composite writes no body and queues its value for a block of its own.
fn push<'a>(
    values: &mut Vec<u8>,
    block: &mut Vec<Entry>,
    pending: &mut Vec<&'a Variant>,
    id: u16,
    value: &'a Variant,
) {
    match value {
        Variant::Map(_) => {
            block.push(Entry {
                id,
                ty: ObjValueType::Obj.to_byte(),
                len: 0,
                composite: true,
            });
            pending.push(value);
        }
        Variant::Array(_) => {
            block.push(Entry {
                id,
                ty: ObjValueType::Array.to_byte(),
                len: 0,
                composite: true,
            });
            pending.push(value);
        }
        scalar => {
            let (ty, len) = write_body(values, scalar);
            block.push(Entry {
                id,
                ty,
                len,
                composite: false,
            });
        }
    }
}

/// Write one leaf body and report `(type, length)`. Widths are minimal
/// big-endian, exactly as the inline frames wrote them: zero-padded unsigned,
/// sign-extended signed, so the reader casts without a width on the wire.
fn write_body(values: &mut Vec<u8>, v: &Variant) -> (u8, usize) {
    let start = values.len();
    let ty = match v {
        Variant::U8(n) => {
            push_uint(values, *n as u64);
            ObjValueType::UInt
        }
        Variant::U16(n) => {
            push_uint(values, *n as u64);
            ObjValueType::UInt
        }
        Variant::U32(n) => {
            push_uint(values, *n as u64);
            ObjValueType::UInt
        }
        Variant::U64(n) => {
            push_uint(values, *n);
            ObjValueType::UInt
        }
        Variant::I64(n) => {
            push_int(values, *n);
            ObjValueType::Int
        }
        Variant::F64(f) => {
            values.extend_from_slice(&f.to_be_bytes());
            ObjValueType::F64
        }
        Variant::Str(s) => {
            values.extend_from_slice(s.as_bytes());
            ObjValueType::Str
        }
        Variant::Bytes(b) => {
            values.extend_from_slice(b);
            ObjValueType::Bytes
        }
        Variant::Bool(b) => {
            values.push(u8::from(*b));
            ObjValueType::Bool
        }
        Variant::Null => ObjValueType::Null,
        Variant::Array(_) | Variant::Map(_) => unreachable!("composites carry no body"),
    };
    (ty.to_byte(), values.len() - start)
}

/// Minimal big-endian unsigned: leading zero bytes dropped, never empty.
fn push_uint(buf: &mut Vec<u8>, v: u64) {
    let be = v.to_be_bytes();
    let first = be.iter().position(|&b| b != 0).unwrap_or(7);
    buf.extend_from_slice(&be[first..]);
}

/// Minimal big-endian two's complement: the shortest width whose sign bit
/// still reproduces the value.
fn push_int(buf: &mut Vec<u8>, v: i64) {
    let be = v.to_be_bytes();
    let mut first = 0usize;
    while first < 7 {
        let sign_ok = (be[first] == 0x00 && be[first + 1] & 0x80 == 0)
            || (be[first] == 0xFF && be[first + 1] & 0x80 != 0);
        if !sign_ok {
            break;
        }
        first += 1;
    }
    buf.extend_from_slice(&be[first..]);
}

/// One entry id: `u8` for `< 0xFF`, else the `0xFF` escape plus a `u16` BE —
/// the same shape the inline frames used, so ids keep their width story.
fn write_id(buf: &mut Vec<u8>, id: u16) {
    if id < 0xFF {
        buf.push(id as u8);
    } else {
        buf.push(0xFF);
        buf.extend_from_slice(&id.to_be_bytes());
    }
}

fn read_id(bytes: &[u8], at: &mut usize) -> Option<u16> {
    let first = *bytes.get(*at)?;
    *at += 1;
    if first == 0xFF {
        let hi = *bytes.get(*at)?;
        let lo = *bytes.get(*at + 1)?;
        *at += 2;
        Some(u16::from_be_bytes([hi, lo]))
    } else {
        Some(first as u16)
    }
}

/// Decode one segment value into its fields, in block order (name order within
/// each block). A malformed table yields an empty list rather than a guess —
/// the same rule the rest of the codec follows: what cannot be decoded is not
/// invented.
pub fn decode(bytes: &[u8], name_of: &mut dyn FnMut(u16) -> Option<String>) -> Vec<DynamicField> {
    if bytes.len() < HEAD {
        return Vec::new();
    }
    let toc_len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let toc_end = HEAD + toc_len;
    if toc_end > bytes.len() {
        return Vec::new();
    }
    let value_area = &bytes[toc_end..];

    let mut blocks: Vec<Vec<Item>> = Vec::new();
    let mut at = HEAD;
    let mut base = 0usize;
    let mut composites = 0usize;
    while at < toc_end {
        let Some((count, n)) = take_len(&bytes[at..]) else {
            return Vec::new();
        };
        at += n;
        let mut items: Vec<Item> = Vec::with_capacity(count.min(256));
        for _ in 0..count {
            let Some(id) = read_id(bytes, &mut at) else {
                return Vec::new();
            };
            let Some(&head) = bytes.get(at) else {
                return Vec::new();
            };
            at += 1;
            let composite = head & COMPOSITE != 0;
            let ty = head & TYPE_MASK;
            if composite {
                composites += 1;
                items.push(Item {
                    id,
                    ty,
                    composite,
                    off: 0,
                    len: 0,
                    child: composites,
                });
            } else {
                // A fixed-width type answers its own length; otherwise the head
                // carries it, or points at the shared varint.
                let len = match implied_len(ty) {
                    Some(len) => len,
                    None => {
                        let code = (head >> LEN_SHIFT) & LEN_ESCAPE;
                        if code == LEN_ESCAPE {
                            let Some((len, n)) = take_len(&bytes[at..]) else {
                                return Vec::new();
                            };
                            at += n;
                            len
                        } else {
                            code as usize
                        }
                    }
                };
                let off = base;
                let Some(next) = base.checked_add(len) else {
                    return Vec::new();
                };
                base = next;
                items.push(Item {
                    id,
                    ty,
                    composite,
                    off,
                    len,
                    child: 0,
                });
            }
        }
        blocks.push(items);
    }
    // Every value byte belongs to exactly one leaf; anything else is a table
    // that disagrees with itself.
    if base != value_area.len() || blocks.len() != composites + 1 {
        return Vec::new();
    }

    fn build(
        blocks: &[Vec<Item>],
        value_area: &[u8],
        name_of: &mut dyn FnMut(u16) -> Option<String>,
        block: usize,
        depth: usize,
    ) -> Option<Vec<(u16, Variant)>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let items = blocks.get(block)?;
        let mut out: Vec<(u16, Variant)> = Vec::with_capacity(items.len());
        for item in items {
            let value = if item.composite {
                let children = build(blocks, value_area, &mut *name_of, item.child, depth + 1)?;
                match ObjValueType::from_byte(item.ty) {
                    Some(ObjValueType::Array) => {
                        Variant::Array(children.into_iter().map(|(_, v)| v).collect())
                    }
                    Some(ObjValueType::Obj) => {
                        let mut map = BTreeMap::new();
                        for (id, v) in children {
                            let key = name_of(id).unwrap_or_else(|| id.to_string());
                            map.insert(key, v);
                        }
                        Variant::Map(map)
                    }
                    // An unknown composite type: skip it, as an unknown leaf is
                    // skipped. Its block is still consumed by the counter.
                    _ => continue,
                }
            } else {
                let end = item.off.checked_add(item.len)?;
                let body = value_area.get(item.off..end)?;
                match decode_body(item.ty, body) {
                    Some(v) => v,
                    None => continue,
                }
            };
            out.push((item.id, value));
        }
        Some(out)
    }

    match build(&blocks, value_area, &mut *name_of, 0, 0) {
        Some(fields) => fields
            .into_iter()
            .map(|(id, value)| DynamicField { id, value })
            .collect(),
        None => Vec::new(),
    }
}

/// Scalar decode: widthless integers widen to `U64`/`I64` (the wire carries no
/// width; a typed field's width comes from its declaration).
fn decode_body(ty: u8, body: &[u8]) -> Option<Variant> {
    Some(match ObjValueType::from_byte(ty)? {
        ObjValueType::UInt => {
            if body.is_empty() || body.len() > 8 {
                return None;
            }
            let mut be = [0u8; 8];
            be[8 - body.len()..].copy_from_slice(body);
            Variant::U64(u64::from_be_bytes(be))
        }
        ObjValueType::Int => {
            if body.is_empty() || body.len() > 8 {
                return None;
            }
            let fill = if body[0] & 0x80 != 0 { 0xFF } else { 0x00 };
            let mut be = [fill; 8];
            be[8 - body.len()..].copy_from_slice(body);
            Variant::I64(i64::from_be_bytes(be))
        }
        ObjValueType::F64 => {
            if body.len() != 8 {
                return None;
            }
            let mut be = [0u8; 8];
            be.copy_from_slice(body);
            Variant::F64(f64::from_be_bytes(be))
        }
        ObjValueType::Str => Variant::Str(std::str::from_utf8(body).ok()?.to_owned()),
        ObjValueType::Bytes => Variant::Bytes(body.to_vec()),
        ObjValueType::Bool => {
            if body.len() != 1 {
                return None;
            }
            Variant::Bool(body[0] != 0)
        }
        ObjValueType::Null => {
            if !body.is_empty() {
                return None;
            }
            Variant::Null
        }
        // Composites carry no body; a leaf entry claiming one is malformed.
        ObjValueType::Array | ObjValueType::Obj => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Variant)]) -> ValueMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn vocab() -> impl FnMut(&str) -> Result<u16, ()> {
        let mut next = 1u16;
        move |_name: &str| {
            let id = next;
            next += 1;
            Ok(id)
        }
    }

    fn round_trip(fields: &ValueMap) -> Vec<(String, Variant)> {
        let mut v = vocab();
        let bytes = encode(fields, &mut v).expect("encode");
        // Flat maps only: names allocate in map order, so id k is names[k-1].
        let names: Vec<String> = fields.keys().cloned().collect();
        decode(&bytes, &mut |id| {
            names.get((id as usize).checked_sub(1)?).cloned()
        })
        .into_iter()
        .map(|f| (names[(f.id as usize) - 1].clone(), f.value))
        .collect()
    }

    #[test]
    fn a_flat_map_round_trips_and_the_table_sits_after_two_bytes() {
        let fields = map(&[
            ("alpha", Variant::U64(7)),
            ("beta", Variant::Str("two".into())),
        ]);
        let mut v = vocab();
        let bytes = encode(&fields, &mut v).expect("encode");
        let toc_len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
        // head + table + "one scalar body each"
        assert_eq!(bytes.len(), HEAD + toc_len + 1 + 3);
        let got = round_trip(&fields);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, Variant::U64(7));
        assert_eq!(got[1].1, Variant::Str("two".into()));
    }

    #[test]
    fn a_nested_object_and_array_keep_their_shape() {
        let mut inner = BTreeMap::new();
        inner.insert("leaf".to_string(), Variant::U64(1));
        let fields = map(&[
            ("obj", Variant::Map(inner)),
            (
                "arr",
                Variant::Array(vec![Variant::U64(1), Variant::Str("x".into())]),
            ),
        ]);
        // Names allocate on first sight, including nested ones, so the table
        // is built while encoding and read back while decoding.
        let mut table: Vec<String> = Vec::new();
        let mut resolver = |name: &str| -> Result<u16, ()> {
            if let Some(at) = table.iter().position(|n| n == name) {
                return Ok((at + 1) as u16);
            }
            table.push(name.to_string());
            Ok(table.len() as u16)
        };
        let bytes = encode(&fields, &mut resolver).expect("encode");
        let snapshot = table.clone();
        let decoded = decode(&bytes, &mut |id| {
            snapshot.get((id as usize).checked_sub(1)?).cloned()
        });
        let mut by_name: BTreeMap<String, Variant> = BTreeMap::new();
        for field in decoded {
            let name = snapshot
                .get((field.id as usize) - 1)
                .cloned()
                .expect("name");
            by_name.insert(name, field.value);
        }
        match by_name.get("obj") {
            Some(Variant::Map(m)) => {
                assert_eq!(m.get("leaf"), Some(&Variant::U64(1)))
            }
            other => panic!("expected Map, got {other:?}"),
        }
        assert_eq!(
            by_name.get("arr"),
            Some(&Variant::Array(vec![
                Variant::U64(1),
                Variant::Str("x".into())
            ]))
        );
    }

    #[test]
    fn a_big_value_among_small_ones_round_trips() {
        let big = vec![0xABu8; 4096];
        let fields = map(&[
            ("a", Variant::U64(1)),
            ("big", Variant::Bytes(big.clone())),
            ("z", Variant::U64(2)),
        ]);
        let got = round_trip(&fields);
        let big_got = got.iter().find(|(n, _)| n == "big").map(|(_, v)| v.clone());
        assert_eq!(big_got, Some(Variant::Bytes(big)));
    }

    #[test]
    fn an_unknown_type_is_skipped_not_fatal() {
        let fields = map(&[("a", Variant::U64(1)), ("b", Variant::U64(2))]);
        let mut v = vocab();
        let mut bytes = encode(&fields, &mut v).expect("encode");
        // Rewrite the first leaf's type byte to an unknown tag (9). The entry is
        // still self-delimiting: its len follows, so the next entry survives.
        let at = HEAD;
        let mut scan = at;
        let (_, n) = take_len(&bytes[scan..]).expect("count");
        scan += n;
        // id, then the head whose low nibble is the type
        scan += 1;
        assert_eq!(bytes[scan] & TYPE_MASK, ObjValueType::UInt.to_byte());
        bytes[scan] = (bytes[scan] & !TYPE_MASK) | 9;
        let mut i = 0usize;
        let decoded = decode(&bytes, &mut |_id| {
            i += 1;
            Some(format!("f{i}"))
        });
        assert_eq!(decoded.len(), 1, "the unknown field is skipped");
        assert_eq!(decoded[0].value, Variant::U64(2));
    }

    #[test]
    fn a_table_that_disagrees_with_its_value_area_is_refused() {
        let fields = map(&[("a", Variant::U64(1))]);
        let mut v = vocab();
        let bytes = encode(&fields, &mut v).expect("encode");
        // One byte of junk after the value area: the lengths no longer add up.
        let mut torn = bytes.clone();
        torn.push(0);
        assert!(decode(&torn, &mut |_id| None).is_empty());
        // A head that claims a table larger than the value.
        let mut lying = bytes.clone();
        lying[0..2].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(decode(&lying, &mut |_id| None).is_empty());
    }

    // ---- layout v3: one head byte (ADR-0060) -------------------------------

    /// id 1, len 1, UInt; id 2, Array; id 3, len 3, Str; id 4, Obj.
    fn nested_example() -> ValueMap {
        let inner = BTreeMap::from([("leaf".to_string(), Variant::I64(-2))]);
        map(&[
            ("alpha", Variant::U64(7)),
            (
                "arr",
                Variant::Array(vec![Variant::Bool(true), Variant::Null]),
            ),
            ("beta", Variant::Str("two".into())),
            ("nest", Variant::Map(inner)),
        ])
    }

    #[test]
    fn the_head_is_one_byte_and_the_nest_stays_two() {
        let fields = nested_example();
        let mut v = vocab();
        let bytes = encode(&fields, &mut v).expect("encode");
        //   head 00 11 (table = 17)  | block 0                          | block 1        | block 2
        let want: &[u8] = &[
            0x00, 0x11, // toc_len
            0x04, // count 4
            0x01, 0x10, // alpha: id 1, head = len 1 << 4 | UInt
            0x02, 0x86, // arr: composite Array — byte-identical to v2
            0x03, 0x32, // beta: id 3, head = len 3 << 4 | Str
            0x04, 0x87, // nest: composite Obj
            0x02, // count 2
            0x00, 0x04, // element: id 0, Bool — length comes from the type
            0x00, 0x05, // element: id 0, Null
            0x01, // count 1
            0x05, 0x18, // leaf: id 5, head = len 1 << 4 | Int
            0x07, // alpha body
            0x74, 0x77, 0x6f, // beta body "two"
            0x01, // true
            0xfe, // -2
        ];
        assert_eq!(bytes, want, "got {bytes:02x?}");
        assert_eq!(bytes.len(), 25, "v2 was 30 for the same map");
    }

    #[test]
    fn the_escape_starts_at_seven_bytes() {
        for (body, head_bytes) in [(6usize, vec![0x01, 0x62]), (7, vec![0x01, 0x72, 0x07])] {
            let fields = map(&[("s", Variant::Str("x".repeat(body)))]);
            let mut v = vocab();
            let bytes = encode(&fields, &mut v).expect("encode");
            // head, then block count, then the entry
            let entry = HEAD + 1;
            assert_eq!(
                &bytes[entry..entry + head_bytes.len()],
                &head_bytes[..],
                "len {body}"
            );
            let toc = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
            assert_eq!(toc, 1 + head_bytes.len(), "table for len {body}");
            assert_eq!(bytes.len(), 2 + toc + body);
        }
    }

    #[test]
    fn a_long_body_escapes_to_the_shared_varint() {
        let fields = map(&[("s", Variant::Bytes(vec![0xAB; 128]))]);
        let mut v = vocab();
        let bytes = encode(&fields, &mut v).expect("encode");
        // head, block count, then id, head (code 7 << 4 | Bytes), put_len(128) = 0x80 0x80.
        assert_eq!(&bytes[3..7], &[0x01, 0x73, 0x80, 0x80]);
        assert_eq!(u16::from_be_bytes([bytes[0], bytes[1]]) as usize, 1 + 4);
    }

    #[test]
    fn the_fixed_width_types_carry_no_length() {
        let fields = map(&[
            ("b", Variant::Bool(false)),
            ("f", Variant::F64(1.5)),
            ("n", Variant::Null),
        ]);
        let mut v = vocab();
        let bytes = encode(&fields, &mut v).expect("encode");
        let mut want = vec![0x00, 0x07, 0x03]; // toc_len 7 (1 + 3 entries of 2), count 3
        want.extend_from_slice(&[0x01, 0x04]); // Bool: head = type only
        want.extend_from_slice(&[0x02, 0x01]); // F64: head = type only
        want.extend_from_slice(&[0x03, 0x05]); // Null: head = type only
        want.push(0x00); // false
        want.extend_from_slice(&1.5f64.to_be_bytes());
        assert_eq!(bytes, want, "got {bytes:02x?}");
    }

    #[test]
    fn an_unknown_type_is_skipped_with_and_without_the_escape() {
        // Inline length: rewrite the type nibble of the first leaf to 9.
        let fields = map(&[("a", Variant::U64(1)), ("b", Variant::U64(2))]);
        let mut v = vocab();
        let mut bytes = encode(&fields, &mut v).expect("encode");
        bytes[4] = 0x19; // id 1, head = len 1 << 4 | unknown 9
        let decoded = decode(&bytes, &mut |id| Some(format!("f{id}")));
        assert_eq!(decoded.len(), 1, "the unknown field is skipped");
        assert_eq!(decoded[0].value, Variant::U64(2));

        // Escaped length: the same nibble with code 7 still tells the reader
        // that a varint follows, so the entry stays skippable.
        let fields = map(&[("a", Variant::Str("x".repeat(9)))]);
        let mut v = vocab();
        let mut bytes = encode(&fields, &mut v).expect("encode");
        bytes[4] = 0x79; // id 1, head = code 7 << 4 | unknown 9
        assert!(decode(&bytes, &mut |_| None).is_empty());
    }
}
