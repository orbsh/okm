//! `Uuid4` / `Uuid7` — 128-bit identifiers, **16 bytes on the wire, never text**.
//!
//! An identifier that is identity belongs in the KEY, and a key is fixed width: a uuid is the
//! 16-byte value, and the hyphenated string is a DISPLAY form (as base64 is for a token). Nothing
//! here stores or encodes text.
//!
//! Which one to use is a storage question, not a taste question:
//!
//! - [`Uuid7`] carries a 48-bit millisecond prefix, so **byte order is time order** — ids minted
//!   in sequence land at the end of the keyspace, and a btree's hot page stays hot. That is what
//!   device / account ids want.
//! - [`Uuid4`] is 128 bits of randomness with no structure. That is what a TOKEN wants: it is a
//!   credential, needs no locality, and must not be predictable from its neighbours (a v7 leaks
//!   its creation millisecond and the ids around it).
//!
//! Layout (RFC 9562), v7: `unix_ts_ms(48) | ver=7(4) | rand_a(12) | var=10(2) | rand_b(62)`.
//! v4: `rand(48) | ver=4(4) | rand(12) | var=10(2) | rand(62)`.
//!
//! No monotonic counter inside one millisecond: two v7 ids minted in the same millisecond sort by
//! their random tail, not by creation order. That is legal v7 and sufficient here — the reason to
//! use v7 is insert locality, not per-millisecond ordering.

use std::fmt;
use std::str::FromStr;

use crate::model::field::{FieldDesc, FieldType};
use crate::model::key::KeyEncode;

const TEXT_LEN: usize = 36;

fn hex_nibble(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        'a'..='f' => Some(c as u8 - b'a' + 10),
        'A'..='F' => Some(c as u8 - b'A' + 10),
        _ => None,
    }
}

/// Parse `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, or the same 32 hex digits with the separators
/// left out — the two spellings a human or a log line produces.
fn from_text(s: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    let mut n = 0usize;
    let mut hi: Option<u8> = None;
    for c in s.chars() {
        if c == '-' {
            continue;
        }
        let v = hex_nibble(c)?;
        match hi {
            None => hi = Some(v),
            Some(h) => {
                if n == 16 {
                    return None;
                }
                out[n] = (h << 4) | v;
                n += 1;
                hi = None;
            }
        }
    }
    (n == 16 && hi.is_none()).then_some(out)
}

fn write_text(b: &[u8; 16], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut buf = String::with_capacity(TEXT_LEN);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            buf.push('-');
        }
        buf.push_str(&format!("{byte:02x}"));
    }
    f.write_str(&buf)
}

fn fill(b: &mut [u8]) {
    // A uuid is a credential (v4) or identity (v7): both need the OS source. There is no
    // fallback to a weaker generator — a caller that cannot get randomness has no uuid.
    getrandom::fill(b).expect("okm: the OS could not produce randomness for a uuid");
}

/// A version-4 uuid: 128 random bits. Use it where the identifier is a CREDENTIAL (a token).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Uuid4([u8; 16]);

impl Uuid4 {
    /// Mint one from the OS source.
    pub fn new() -> Self {
        let mut b = [0u8; 16];
        fill(&mut b);
        b[6] = (b[6] & 0x0f) | 0x40; // version 4
        b[8] = (b[8] & 0x3f) | 0x80; // variant 10
        Self(b)
    }

    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// A version-7 uuid: a millisecond prefix followed by randomness. Use it where the identifier is
/// IDENTITY that will be inserted in order (device, account) — byte order is time order, so the
/// keyspace keeps its writes at the tail.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Uuid7([u8; 16]);

impl Uuid7 {
    /// Mint one stamped with now.
    pub fn now() -> Self {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self::with_millis(ms)
    }

    /// Mint one stamped with an explicit millisecond clock (a replay, a fixture, a caller whose
    /// clock is the one that matters).
    pub fn with_millis(ms: u64) -> Self {
        let mut b = [0u8; 16];
        b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
        fill(&mut b[6..]);
        b[6] = (b[6] & 0x0f) | 0x70; // version 7
        b[8] = (b[8] & 0x3f) | 0x80; // variant 10
        Self(b)
    }

    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The millisecond the id was minted at.
    pub fn millis(&self) -> u64 {
        let mut ms = [0u8; 8];
        ms[2..].copy_from_slice(&self.0[..6]);
        u64::from_be_bytes(ms)
    }
}

impl fmt::Display for Uuid4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_text(&self.0, f)
    }
}

impl fmt::Debug for Uuid4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_text(&self.0, f)
    }
}

impl fmt::Display for Uuid7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_text(&self.0, f)
    }
}

impl fmt::Debug for Uuid7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_text(&self.0, f)
    }
}

impl FromStr for Uuid4 {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        from_text(s)
            .map(Self)
            .ok_or_else(|| format!("not a uuid: {s}"))
    }
}

impl FromStr for Uuid7 {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        from_text(s)
            .map(Self)
            .ok_or_else(|| format!("not a uuid: {s}"))
    }
}

impl From<[u8; 16]> for Uuid4 {
    fn from(b: [u8; 16]) -> Self {
        Self(b)
    }
}

impl From<[u8; 16]> for Uuid7 {
    fn from(b: [u8; 16]) -> Self {
        Self(b)
    }
}

/// A uuid IS the key: 16 fixed bytes, one field, no wrapper struct and no payload mirror. The
/// declaration order is the single field, so `ok_head` prefixes work as they do for any key.
const UUID_FIELDS: &[FieldDesc] = &[FieldDesc {
    name: "uuid",
    ty: FieldType::FixedBytes,
    width: 16,
}];

impl KeyEncode for Uuid4 {
    const KEY_LEN: usize = 16;
    const FIELD_WIDTHS: &'static [(&'static str, usize)] = &[("uuid", 16)];
    const FIELDS: &'static [FieldDesc] = UUID_FIELDS;
    fn encode(&self) -> Vec<u8> {
        self.0.to_vec()
    }
    fn decode(bytes: &[u8]) -> Self {
        let mut b = [0u8; 16];
        b.copy_from_slice(&bytes[..16]);
        Self(b)
    }
    fn encode_prefix_named(&self, buf: &mut Vec<u8>, names: &[&str]) -> usize {
        // One field: an empty prefix encodes nothing; naming it encodes the whole id.
        if names.is_empty() {
            return 0;
        }
        buf.extend_from_slice(&self.0);
        16
    }
    fn prefix_width(names: &[&str]) -> usize {
        if names.is_empty() { 0 } else { 16 }
    }
}

impl KeyEncode for Uuid7 {
    const KEY_LEN: usize = 16;
    const FIELD_WIDTHS: &'static [(&'static str, usize)] = &[("uuid", 16)];
    const FIELDS: &'static [FieldDesc] = UUID_FIELDS;
    fn encode(&self) -> Vec<u8> {
        self.0.to_vec()
    }
    fn decode(bytes: &[u8]) -> Self {
        let mut b = [0u8; 16];
        b.copy_from_slice(&bytes[..16]);
        Self(b)
    }
    fn encode_prefix_named(&self, buf: &mut Vec<u8>, names: &[&str]) -> usize {
        // One field: an empty prefix encodes nothing; naming it encodes the whole id.
        if names.is_empty() {
            return 0;
        }
        buf.extend_from_slice(&self.0);
        16
    }
    fn prefix_width(names: &[&str]) -> usize {
        if names.is_empty() { 0 } else { 16 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_and_the_layout_is_rfc_shaped() {
        let u = Uuid7::with_millis(0x0102_0304_0506);
        let s = u.to_string();
        assert_eq!(s.len(), TEXT_LEN);
        assert_eq!(&s[..13], "01020304-0506");
        assert_eq!(&s[14..15], "7", "version nibble");
        assert_eq!(s.parse::<Uuid7>().unwrap(), u);
        // The separator-free spelling is the same value.
        assert_eq!(s.replace('-', "").parse::<Uuid7>().unwrap(), u);
        assert_eq!(u.millis(), 0x0102_0304_0506);
    }

    #[test]
    fn v7_byte_order_is_time_order() {
        let a = Uuid7::with_millis(1_000);
        let b = Uuid7::with_millis(2_000);
        assert!(a.as_bytes() < b.as_bytes(), "later millisecond sorts later");
    }

    #[test]
    fn v4_is_random_and_versioned() {
        let a = Uuid4::new();
        let b = Uuid4::new();
        assert_ne!(a, b);
        assert_eq!(a.as_bytes()[6] >> 4, 4);
        assert_eq!(a.as_bytes()[8] >> 6, 0b10);
    }
}
