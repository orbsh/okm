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
//! write frame: [op_count len-enc] per op: [tag u8][key LK][value LV][key][value]
//! read frame:  [op_count len-enc] per op: [tag u8][key LK][value LV][key]
//! response:    [has_value u8][value len LK][value bytes] [count len-enc] per hit: [len][bytes]
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

/// Op tags: 2 bits used, 2 reserved (high nibble of the op header byte).
pub const OP_PUT: u8 = 0;
pub const OP_DELETE: u8 = 1;
pub const OP_GET: u8 = 2;
pub const OP_SCAN: u8 = 3;

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

// ---- write frames ----

/// A write frame: `[op_count][op]...` — the batch's op list as
/// `(tag, key, value)` triples. One frame = one receiver WAL commit
/// (ADR-0010 §2): the atomicity boundary maps 1:1 from the sender's
/// batch to the receiver's engine.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct WriteFrame(pub Vec<(u8, Vec<u8>, Vec<u8>)>);

impl WriteFrame {
    pub fn new(ops: Vec<(u8, Vec<u8>, Vec<u8>)>) -> Self {
        Self(ops)
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
            if tag > OP_SCAN {
                return None;
            }
            let key = take(frame, &mut pos, klen)?;
            let value = take(frame, &mut pos, vlen)?;
            ops.push((tag, key, value));
        }
        Some(Self(ops))
    }
}

// ---- read frames ----

/// A read request frame: one point read or one prefix scan. The response
/// travels back out-of-band — correlation is the transport's business
/// (envelope address, connection id), never the frame's (ADR-0010 §6).
#[derive(Debug, Clone, PartialEq)]
pub enum ReadFrame {
    Get { key: Vec<u8> },
    Scan { prefix: Vec<u8> },
}

impl ReadFrame {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        put_len(&mut buf, 1);
        let (tag, key) = match self {
            ReadFrame::Get { key } => (OP_GET, key),
            ReadFrame::Scan { prefix } => (OP_SCAN, prefix),
        };
        put_op_header(&mut buf, tag, key.len(), 0);
        buf.extend_from_slice(key);
        buf
    }

    pub fn decode(frame: &[u8]) -> Option<Self> {
        let mut pos = 0;
        let count = get_len(frame, &mut pos)?;
        if count != 1 {
            return None; // a read frame carries exactly one op
        }
        let (tag, klen, vlen) = get_op_header(frame, &mut pos)?;
        if vlen != 0 {
            return None;
        }
        let key = take(frame, &mut pos, klen)?;
        match tag {
            OP_GET => Some(ReadFrame::Get { key }),
            OP_SCAN => Some(ReadFrame::Scan { prefix: key }),
            _ => None,
        }
    }
}

/// The receiver's answer to one read frame. Raw bytes in, raw bytes out —
/// the receiver never learns what the bytes mean.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ReadResponse {
    /// Point read answer (`None` = key absent).
    pub value: Option<Vec<u8>>,
    /// Scan answer: key suffixes relative to the requested prefix — the
    /// engine's own `scan_suffix` contract, byte-for-byte, so the sender
    /// re-splits exactly as it would against a local engine.
    pub suffixes: Vec<Vec<u8>>,
}

impl ReadResponse {
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
        put_len(&mut buf, self.suffixes.len());
        for sfx in &self.suffixes {
            put_len(&mut buf, sfx.len());
            buf.extend_from_slice(sfx);
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
        Some(Self { value, suffixes })
    }
}
