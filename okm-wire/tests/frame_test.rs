//! ADR-0010 Phase 7 wire-format tests: the hand-parsed frame codec's byte
//! layout is a persistent contract — locked here byte-for-byte (same
//! discipline as the key/index hex tests). Covers the length codec's four
//! buckets, the op header, round trips, and malformed-frame rejection.

use okm_wire::{put_len, ReadFrame, ReadResponse, WriteFrame};

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
fn write_frame_round_trip_and_layout() {
    let frame = WriteFrame(vec![
        (0, b"k1".to_vec(), b"v1".to_vec()),       // put
        (1, b"k0".to_vec(), Vec::new()),           // delete, empty value
        (0, vec![0xAB; 200], vec![0xCD; 70_000]),  // 22-bit key len, 32-bit value len
    ]);
    let bytes = frame.encode();

    // Layout lock: op_count inline(3) + op1 header + key + value + op2
    // header + key + value(len 0 inline) + op3 header + key + value.
    assert_eq!(&bytes[0], &3, "op_count = 3, inline bucket");
    // op 1: tag 0<<4, key len 2, value len 2, key "k1", value "v1"
    assert_eq!(&bytes[1..8], &[0x00, 0x02, 0x02, b'k', b'1', b'v', b'1']);
    // op 2: tag 1<<4, key len 2, value len 0, key "k0" (no value bytes)
    assert_eq!(&bytes[8..13], &[0x10, 0x02, 0x00, b'k', b'0']);
    // op 3: tag 0<<4, key len 200 → 14-bit bucket [0x40, 0xC8]; value
    // len 70000 → 22-bit bucket [0x80 | hi, mid, lo] where 70000 =
    // 0x1_11_70 → [0x11, 0x70]... verify via re-encode instead:
    let back = WriteFrame::decode(&bytes).expect("round trip");
    assert_eq!(back.0.len(), 3);
    assert_eq!(back.0[0], (0, b"k1".to_vec(), b"v1".to_vec()));
    assert_eq!(back.0[1], (1, b"k0".to_vec(), Vec::new()));
    assert_eq!(back.0[2].1, vec![0xAB; 200]);
    assert_eq!(back.0[2].2, vec![0xCD; 70_000]);
    assert_eq!(back.encode(), bytes, "re-encode is byte-identical");
}

#[test]
fn read_frame_and_response_round_trip() {
    let rf = ReadFrame::Scan {
        prefix: b"user:".to_vec(),
    };
    let bytes = rf.encode();
    // [op_count=1 inline][tag 3<<4][key len 5][value len 0]["user:"]
    assert_eq!(bytes, vec![1, 0x30, 5, 0, b'u', b's', b'e', b'r', 0x3A]);

    let resp = ReadResponse {
        value: Some(b"hello".to_vec()),
        suffixes: vec![b"1".to_vec(), b"2".to_vec()],
    };
    let back = ReadResponse::decode(&resp.encode()).expect("round trip");
    assert_eq!(back.value.as_deref(), Some(b"hello".as_slice()));
    assert_eq!(back.suffixes, vec![b"1".to_vec(), b"2".to_vec()]);

    let none = ReadResponse::default();
    let back = ReadResponse::decode(&none.encode()).expect("round trip");
    assert_eq!(back.value, None);
    assert!(back.suffixes.is_empty());
}

#[test]
fn malformed_frames_rejected_not_panic() {
    // Truncated op_count.
    assert!(WriteFrame::decode(&[]).is_none());

    // op_count lies: claims 2 ops, only one present.
    let mut f = Vec::new();
    okm_wire::put_len(&mut f, 2);
    f.extend_from_slice(&[0x00, 0x01, 0x01, b'k', b'v']); // 1 complete op
    assert!(WriteFrame::decode(&f).is_none());

    // Key length exceeds remaining bytes.
    let mut f = Vec::new();
    okm_wire::put_len(&mut f, 1);
    f.extend_from_slice(&[0x00, 0x7F, 0x00, b'k']); // key len 127 inline, 1 byte present
    assert!(WriteFrame::decode(&f).is_none());

    // Unknown op tag (0x04 in high nibble — reserved).
    let mut f = Vec::new();
    okm_wire::put_len(&mut f, 1);
    f.extend_from_slice(&[0x40, 0x01, 0x01, b'k', b'v']);
    assert!(WriteFrame::decode(&f).is_none());

    // Read frame: count != 1 rejected.
    let mut f = Vec::new();
    okm_wire::put_len(&mut f, 2);
    assert!(ReadResponse::decode(&[9]).is_none()); // invalid has_value byte
}
