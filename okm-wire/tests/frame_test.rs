//! ADR-0010 Phase 7 wire-format tests: the hand-parsed frame codec's byte
//! layout is a persistent contract — locked here byte-for-byte (same
//! discipline as the key/index hex tests). Covers the length codec's four
//! buckets, the op header, round trips, and malformed-frame rejection.

use okm_wire::{put_len, OpFrame, OpResponse, OP_SCAN_STREAM, TAIL_FINAL, TAIL_MORE};

#[test]
fn length_codec_four_buckets() {
    // inline (≤63): one byte, high 2 bits = 00
    let mut b = Vec::new();
    put_len(&mut b, 63);
    assert_eq!(b, vec![0x00 | 63]);
    b.clear();
    put_len(&mut b, 0);
    assert_eq!(b, vec![0]);

    // 14-bit: 2 bytes, high 2 bits = 01
    b.clear();
    put_len(&mut b, 64);
    assert_eq!(b, vec![0b0100_0000, 64]);
    b.clear();
    put_len(&mut b, 0x1234);
    assert_eq!(b, vec![0b0100_0000 | 0x12, 0x34]);

    // 22-bit: 3 bytes, high 2 bits = 10
    b.clear();
    put_len(&mut b, 1 << 14);
    assert_eq!(b, vec![0b1000_0000, 0x40, 0x00]);
    b.clear();
    put_len(&mut b, 0x2A_BC_DE);
    assert_eq!(b, vec![0b1000_0000 | 0x2A, 0xBC, 0xDE]);

    // 32-bit: 5 bytes, high 2 bits = 11
    b.clear();
    put_len(&mut b, 1 << 22);
    assert_eq!(b, vec![0b1100_0000, 0x00, 0x40, 0x00, 0x00]);
    b.clear();
    put_len(&mut b, 0x12_34_56_78);
    assert_eq!(b, vec![0b1100_0000, 0x12, 0x34, 0x56, 0x78]);
}

#[test]
fn frame_round_trip_and_layout() {
    let frame = OpFrame::new(vec![
        (0, b"k1".to_vec(), b"v1".to_vec()),      // put
        (1, b"k0".to_vec(), Vec::new()),          // delete, empty value
        (2, b"get-me".to_vec(), Vec::new()),      // get: value empty (LV=0)
        (3, b"scan-me".to_vec(), Vec::new()),     // scan: value empty (LV=0)
        (0, vec![0xAB; 200], vec![0xCD; 70_000]), // 22-bit key len, 32-bit value len
    ]);
    let bytes = frame.encode();

    // Layout lock: op_count inline(5) + op1 header + key + value + op2
    // header + key + op3 header + key + op4 header + key + op5 ...
    assert_eq!(&bytes[0], &5, "op_count = 5, inline bucket");
    // op 1: tag 0<<4, key len 2, value len 2, key "k1", value "v1"
    assert_eq!(&bytes[1..8], &[0x00, 0x02, 0x02, b'k', b'1', b'v', b'1']);
    // op 2: tag 1<<4, key len 2, value len 0, key "k0" (no value bytes)
    assert_eq!(&bytes[8..13], &[0x10, 0x02, 0x00, b'k', b'0']);
    // op 3: tag 2<<4 (get), key len 6, value len 0, key "get-me"
    assert_eq!(&bytes[13..22], &[0x20, 0x06, 0x00, b'g', b'e', b't', b'-', b'm', b'e']);
    // op 4: tag 3<<4 (scan), key len 7, value len 0, key "scan-me"
    assert_eq!(&bytes[22..32], &[0x30, 0x07, 0x00, b's', b'c', b'a', b'n', b'-', b'm', b'e']);

    let back = OpFrame::decode(&bytes).expect("round trip");
    assert_eq!(back.0.len(), 5);
    assert_eq!(back.0[0], (0, b"k1".to_vec(), b"v1".to_vec()));
    assert_eq!(back.0[1], (1, b"k0".to_vec(), Vec::new()));
    assert_eq!(back.0[2], (2, b"get-me".to_vec(), Vec::new()));
    assert_eq!(back.0[3], (3, b"scan-me".to_vec(), Vec::new()));
    assert_eq!(back.0[4].1, vec![0xAB; 200]);
    assert_eq!(back.0[4].2, vec![0xCD; 70_000]);
    assert_eq!(back.encode(), bytes, "re-encode is byte-identical");
}

#[test]
fn response_round_trip() {
    let resp = OpResponse {
        value: Some(b"hello".to_vec()),
        suffixes: vec![b"1".to_vec(), b"2".to_vec()],
        hits: Vec::new(),
        tail: None,
    };
    let back = OpResponse::decode(&resp.encode()).expect("round trip");
    assert_eq!(back.value.as_deref(), Some(b"hello".as_slice()));
    assert_eq!(back.suffixes, vec![b"1".to_vec(), b"2".to_vec()]);
    assert_eq!(back.hits, Vec::new());
    assert_eq!(back.tail, None);

    // Put/delete answer = default (empty) response.
    let none = OpResponse::default();
    let back = OpResponse::decode(&none.encode()).expect("round trip");
    assert_eq!(back.value, None);
    assert!(back.suffixes.is_empty());
}

/// ADR-0021: OP_SCAN_STREAM chunk grammar — a plain OpResponse whose hits
/// section carries (key, value) pairs, plus one trailing tail byte.
#[test]
fn stream_chunk_round_trip() {
    let chunk = OpResponse {
        value: None,
        suffixes: Vec::new(),
        hits: vec![
            (b"k1".to_vec(), b"v1".to_vec()),
            (b"k2".to_vec(), b"v2".to_vec()),
        ],
        tail: Some(TAIL_MORE),
    };
    let bytes = chunk.encode();
    // Layout lock: [has_value 0][count 2][klen 2]"k1"[vlen 2]"v1"[klen 2]"k2"[vlen 2]"v2"[tail 0x00]
    assert_eq!(
        &bytes,
        &[0, 2, 2, b'k', b'1', 2, b'v', b'1', 2, b'k', b'2', 2, b'v', b'2', 0x00]
    );
    let back = OpResponse::decode_chunk(&bytes).expect("chunk round trip");
    assert_eq!(back.hits, chunk.hits);
    assert_eq!(back.tail, Some(TAIL_MORE));
    assert_eq!(back.encode(), bytes, "re-encode is byte-identical");

    // Final chunk: tail 0x01, count may be anything (including 0 — the
    // empty range answer).
    let empty_final = OpResponse {
        value: None,
        suffixes: Vec::new(),
        hits: Vec::new(),
        tail: Some(TAIL_FINAL),
    };
    let bytes = empty_final.encode();
    assert_eq!(&bytes, &[0, 0, 0x01]);
    let back = OpResponse::decode_chunk(&bytes).expect("empty final chunk");
    assert!(back.hits.is_empty());
    assert_eq!(back.tail, Some(TAIL_FINAL));

    // Unknown trailer byte and truncated frame reject, never panic.
    let mut bad = empty_final.encode();
    *bad.last_mut().unwrap() = 0x02;
    assert!(OpResponse::decode_chunk(&bad).is_none());
    assert!(OpResponse::decode_chunk(&[0, 0]).is_none());

    // The plain decoder rejects a chunk (trailing tail byte = garbage to
    // it) and vice versa — the two shapes are call-site chosen, not sniffed.
    assert!(OpResponse::decode(&chunk.encode()).is_none());
    let plain = OpResponse {
        value: None,
        suffixes: vec![b"s".to_vec()],
        hits: Vec::new(),
        tail: None,
    };
    assert!(OpResponse::decode_chunk(&plain.encode()).is_none());
}

/// ADR-0021: tag 4 (OP_SCAN_STREAM) enters the frame grammar; tag 5 is
/// still reserved and rejected.
#[test]
fn scan_stream_tag_accepted_next_tag_still_rejected() {
    let frame = OpFrame::one(OP_SCAN_STREAM, b"begin".to_vec(), vec![0x00, 0x10]);
    let bytes = frame.encode();
    // op header: tag 4<<4, key len 5, value len 2.
    assert_eq!(&bytes[1..4], &[0x40, 0x05, 0x02]);
    let back = OpFrame::decode(&bytes).expect("tag 4 decodes");
    assert_eq!(back.0[0].0, OP_SCAN_STREAM);

    let mut f = Vec::new();
    put_len(&mut f, 1);
    f.extend_from_slice(&[0x50, 0x01, 0x01, b'k', b'v']); // tag 5 — reserved
    assert!(OpFrame::decode(&f).is_none(), "tag 5 stays reserved");
}

#[test]
fn malformed_frames_rejected_not_panic() {
    // Truncated op_count.
    assert!(OpFrame::decode(&[]).is_none());

    // op_count lies: claims 2 ops, only one present.
    let mut f = Vec::new();
    put_len(&mut f, 2);
    f.extend_from_slice(&[0x00, 0x01, 0x01, b'k', b'v']); // 1 complete op
    assert!(OpFrame::decode(&f).is_none());

    // Key length exceeds remaining bytes.
    let mut f = Vec::new();
    put_len(&mut f, 1);
    f.extend_from_slice(&[0x00, 0x7F, 0x00, b'k']); // key len 127 inline, 1 byte present
    assert!(OpFrame::decode(&f).is_none());

    // Reserved op tag (0x05 in high nibble — tag 4 is OP_SCAN_STREAM now,
    // ADR-0021; the reserved space moved with it).
    let mut f = Vec::new();
    put_len(&mut f, 1);
    f.extend_from_slice(&[0x50, 0x01, 0x01, b'k', b'v']);
    assert!(OpFrame::decode(&f).is_none());

    assert!(OpResponse::decode(&[9]).is_none()); // invalid has_value byte
}
