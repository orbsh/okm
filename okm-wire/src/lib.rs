//! okm-wire — the VirtualStorage wire-format codec (ADR-0010 §2).
//!
//! Zero dependencies, zero OKM semantics, zero I/O: this crate only knows
//! how to turn a batch of ops into bytes and back. The frame is counted
//! fields, not a protocol — ops are already encoded at the sender's trait
//! boundary, so the wire marks op boundaries with a 2-bit tag nibble and
//! bucketed length prefixes; keys/values cross as opaque bytes.
//!
//! Reuse contract: any transport that can carry one `Vec<u8>` per frame
//! (mpsc, WebSocket message, UDS datagram) needs exactly these encode/
//! decode functions. Byte-stream transports (raw TCP) put the framing in
//! their own layer — the codec does not restate a total-length header
//! (the transport already delimits; restating it is the same fact stored
//! twice).
//!
//! Layout (locked by hex tests):
//!
//! ```text
//! request frame: [op_count len-enc] per op: [tag u8][key LK][value LV][key][value]
//!   all five ops in one shape — put/delete carry their bytes, get/scan/scan-stream
//!   carry value = empty (LV = 0; scan-stream's value segment carries its request
//!   grammar, encoded sender-side); a frame may mix mutating and query ops
//! response frame: [has_value u8][value len LK][value bytes] [count len-enc] per hit: [len][bytes]
//!   put/delete answers are the default (empty) response
//!   OP_SCAN_STREAM chunk: same shape with the hits section carrying
//!   [len][key][len][value] pairs and one trailing tail byte
//!   (0x00 more chunks / 0x01 final); responses without the section decode as
//!   hits = [] / tail = None — one type, two shapes (ADR-0021).
//! length LK/LV — 4 width buckets in the first byte (high 2 bits select,
//! low 6 bits are the value's high bits):
//!   00xxxxxx inline ≤63 | 01xxxxxx+1B 14-bit | 10xxxxxx+2B 22-bit | 11xxxxxx+4B 32-bit
//! ```
//!
//! Malformed frames (lengths exceeding remaining bytes, unknown tags) are
//! rejected with `None`, never panicked — malformed input is normal
//! input. Value compression belongs to the value side (field wrappers
//! know the semantics), never to the frame: compressed bytes are just
//! shorter bytes here.

/// Op tags: 3 used, 1 reserved (high nibble of the op header byte).
pub const OP_PUT: u8 = 0;
pub const OP_DELETE: u8 = 1;
pub const OP_GET: u8 = 2;
pub const OP_SCAN: u8 = 3;
/// Paged streaming scan (ADR-0021): request = scan bounds + page size in
/// the value segment (encoded by the sender, opaque here); response = a
/// chunk (`OpResponse` hits + tail byte). A receiver without this tag
/// rejects the frame; the sender falls back to buffered `OP_SCAN`.
pub const OP_SCAN_STREAM: u8 = 4;

/// Chunk trailer (ADR-0021): more chunks follow this one.
pub const TAIL_MORE: u8 = 0x00;
/// Chunk trailer (ADR-0021): final chunk — the entries just read are all.
pub const TAIL_FINAL: u8 = 0x01;

/// Append `value` with its bucketed length prefix.
pub fn put_len(buf: &mut Vec<u8>, value: usize) {
    debug_assert!(value <= u32::MAX as usize);
    match value {
        v if v < 1 << 6 => buf.push(LEN_INLINE << 6 | v as u8),
        v if v < 1 << 14 => {
            buf.push(LEN_U8 << 6 | (v >> 8) as u8);
            buf.push(v as u8);
        }
        v if v < 1 << 22 => {
            buf.push(LEN_U16 << 6 | (v >> 16) as u8);
            buf.extend_from_slice(&(v as u16).to_be_bytes());
        }
        v => {
            buf.push(LEN_U32 << 6);
            buf.extend_from_slice(&(v as u32).to_be_bytes());
        }
    }
}

/// Read one bucketed length; `None` = truncated or malformed frame.
pub fn get_len(frame: &[u8], pos: &mut usize) -> Option<usize> {
    if *pos >= frame.len() {
        return None;
    }
    let first = frame[*pos];
    *pos += 1;
    let selector = first >> 6;
    // low 6 bits are the value's HIGH bits — shift them up per bucket.
    Some(match selector {
        LEN_INLINE => (first & 0x3f) as usize,
        LEN_U8 => {
            let rest = *frame.get(*pos)? as usize;
            *pos += 1;
            ((first & 0x3f) as usize) << 8 | rest
        }
        LEN_U16 => {
            let rest = u16::from_be_bytes(frame.get(*pos..)?.get(..2)?.try_into().ok()?) as usize;
            *pos += 2;
            ((first & 0x3f) as usize) << 16 | rest
        }
        LEN_U32 => {
            let rest = u32::from_be_bytes(frame.get(*pos..)?.get(..4)?.try_into().ok()?) as usize;
            *pos += 4;
            rest
        }
        _ => unreachable!(),
    })
}

const LEN_INLINE: u8 = 0b00;
const LEN_U8: u8 = 0b01;
const LEN_U16: u8 = 0b10;
const LEN_U32: u8 = 0b11;

/// Write one op header: tag (high nibble) + key length + value length.
pub fn put_op_header(buf: &mut Vec<u8>, tag: u8, key_len: usize, value_len: usize) {
    buf.push(tag << 4);
    put_len(buf, key_len);
    put_len(buf, value_len);
}

/// Read one op header; `None` = malformed frame.
pub fn get_op_header(frame: &[u8], pos: &mut usize) -> Option<(u8, usize, usize)> {
    if *pos >= frame.len() {
        return None;
    }
    let byte = frame[*pos];
    *pos += 1;
    let tag = byte >> 4;
    let key_len = get_len(frame, pos)?;
    let value_len = get_len(frame, pos)?;
    Some((tag, key_len, value_len))
}

/// Take `n` bytes as an owned vector; `None` = truncated frame.
pub fn take(frame: &[u8], pos: &mut usize, n: usize) -> Option<Vec<u8>> {
    let end = pos.checked_add(n)?;
    if end > frame.len() {
        return None;
    }
    let v = frame[*pos..end].to_vec();
    *pos = end;
    Some(v)
}

// ---- unified op frames ----

/// One op: `(tag, key, value)`. All five ops share this shape — reads
/// carry `value = Vec::new()` (LV = 0) except OP_SCAN_STREAM, whose
/// value segment carries its request grammar (bounds + page size,
/// encoded sender-side). Tags: put / delete / get / scan / scan-stream
/// (3 used, 1 reserved).
pub type Op = (u8, Vec<u8>, Vec<u8>);

/// A request frame: `[op_count len-enc] per op: [tag u8][key LK][value LV][key][value]`.
/// All four ops in one shape — a frame is a batch of ops, each op either
/// mutates (put/delete) or queries (get/scan). One frame = one receiver
/// execution pass; mutating ops commit together (one WAL write, the
/// sender's batch atomicity, ADR-0010 §2); query ops run after them in
/// frame order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct OpFrame(pub Vec<Op>);

impl OpFrame {
    pub fn new(ops: Vec<Op>) -> Self {
        Self(ops)
    }

    /// Convenience: single-op frame.
    pub fn one(tag: u8, key: Vec<u8>, value: Vec<u8>) -> Self {
        Self(vec![(tag, key, value)])
    }

    /// MemBatch-shaped ops (None = delete) → one write frame — the
    /// ADR-0010 §2 mapping, shared by the remote backend's
    /// `commit_batch` and the binding plan surface (a planned put IS a
    /// batch: primary + index entries + acc updates in one frame).
    pub fn write_batch(ops: &[(Vec<u8>, Option<Vec<u8>>)]) -> Self {
        Self(
            ops.iter()
                .map(|(k, v)| match v {
                    Some(v) => (OP_PUT, k.clone(), v.clone()),
                    None => (OP_DELETE, k.clone(), Vec::new()),
                })
                .collect(),
        )
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        put_len(&mut buf, self.0.len());
        for (tag, key, value) in &self.0 {
            put_op_header(&mut buf, *tag, key.len(), value.len());
            buf.extend_from_slice(key);
            buf.extend_from_slice(value);
        }
        buf
    }

    /// Decode + validate: lengths exceeding the remaining bytes or an
    /// unknown op tag reject the whole frame.
    pub fn decode(frame: &[u8]) -> Option<Self> {
        let mut pos = 0;
        let count = get_len(frame, &mut pos)?;
        let mut ops = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let (tag, klen, vlen) = get_op_header(frame, &mut pos)?;
            if tag > OP_SCAN_STREAM {
                return None;
            }
            let key = take(frame, &mut pos, klen)?;
            let value = take(frame, &mut pos, vlen)?;
            ops.push((tag, key, value));
        }
        Some(Self(ops))
    }
}

/// The receiver's answer to one request frame. Uniform shape for all
/// ops — a put/delete answer is the default (empty) response: the
/// sender learns success by receiving A response (the transport's
/// delivery, not the frame, carries the guarantee). Raw bytes in, raw
/// bytes out — the receiver never learns what the bytes mean.
///
/// One type, two shapes (ADR-0021): a plain answer has `suffixes`
/// filled (OP_SCAN) and `hits`/`tail` empty; an OP_SCAN_STREAM chunk
/// has `hits` + `tail` filled and `suffixes` empty. `decode` and
/// `encode` keep the two apart by the optional trailing section, so
/// the chunk is exactly "today's `OpResponse` plus one appended byte".
#[derive(Debug, Default, Clone, PartialEq)]
pub struct OpResponse {
    /// Get answer (`None` = key absent). Empty for the other ops.
    pub value: Option<Vec<u8>>,
    /// Scan answer: key suffixes relative to the requested prefix — the
    /// engine's own `scan_suffix` contract, byte-for-byte, so the sender
    /// re-splits exactly as it would against a local engine. Empty for
    /// the other ops.
    pub suffixes: Vec<Vec<u8>>,
    /// OP_SCAN_STREAM chunk: `(key, value)` pairs of this page — the
    /// engine's full-key items, receiver-prefix already applied (the
    /// same bytes a local engine's iterator yields).
    pub hits: Vec<(Vec<u8>, Vec<u8>)>,
    /// OP_SCAN_STREAM chunk trailer: `Some(TAIL_MORE)` / `Some(TAIL_FINAL)`
    /// marks the bytes as a stream chunk; `None` = plain (non-chunk) answer.
    pub tail: Option<u8>,
}

impl OpResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match &self.value {
            Some(v) => {
                buf.push(1);
                put_len(&mut buf, v.len());
                buf.extend_from_slice(v);
            }
            None => buf.push(0),
        }
        // Chunk grammar (tail = Some): hits are (key, value) pairs;
        // plain grammar: suffixes are bare byte strings.
        match self.tail {
            Some(tail) => {
                put_len(&mut buf, self.hits.len());
                for (k, v) in &self.hits {
                    put_len(&mut buf, k.len());
                    buf.extend_from_slice(k);
                    put_len(&mut buf, v.len());
                    buf.extend_from_slice(v);
                }
                buf.push(tail);
            }
            None => {
                put_len(&mut buf, self.suffixes.len());
                for sfx in &self.suffixes {
                    put_len(&mut buf, sfx.len());
                    buf.extend_from_slice(sfx);
                }
            }
        }
        buf
    }

    pub fn decode(frame: &[u8]) -> Option<Self> {
        let mut pos = 0;
        let value = match frame.first()? {
            0 => {
                pos += 1;
                None
            }
            1 => {
                pos += 1;
                let len = get_len(frame, &mut pos)?;
                Some(take(frame, &mut pos, len)?)
            }
            _ => return None,
        };
        let count = get_len(frame, &mut pos)?;
        let mut suffixes = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let len = get_len(frame, &mut pos)?;
            suffixes.push(take(frame, &mut pos, len)?);
        }
        if pos != frame.len() {
            return None; // trailing garbage
        }
        Some(Self {
            value,
            suffixes,
            hits: Vec::new(),
            tail: None,
        })
    }

    /// Decode an `OP_SCAN_STREAM` chunk (ADR-0021 grammar: entries as
    /// `(key, value)` pairs + the trailing tail byte). The sender knows
    /// which op it issued, so the shape is chosen at the call site —
    /// never sniffed from the bytes (a sniffed distinction is ambiguous:
    /// a plain suffix list of the right parity can masquerade as pairs).
    pub fn decode_chunk(frame: &[u8]) -> Option<Self> {
        let mut pos = 0;
        let value = match frame.first()? {
            0 => {
                pos += 1;
                None
            }
            1 => {
                pos += 1;
                let len = get_len(frame, &mut pos)?;
                Some(take(frame, &mut pos, len)?)
            }
            _ => return None,
        };
        let count = get_len(frame, &mut pos)?;
        let mut hits = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let klen = get_len(frame, &mut pos)?;
            let k = take(frame, &mut pos, klen)?;
            let vlen = get_len(frame, &mut pos)?;
            let v = take(frame, &mut pos, vlen)?;
            hits.push((k, v));
        }
        let tail = *frame.get(pos)?;
        if !matches!(tail, TAIL_MORE | TAIL_FINAL) || pos + 1 != frame.len() {
            return None; // unknown trailer or trailing garbage
        }
        Some(Self {
            value,
            suffixes: Vec::new(),
            hits,
            tail: Some(tail),
        })
    }
}

