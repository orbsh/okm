
use okm_core::{KeyEncode, DocumentEncode, Document};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
#[ok_layout(version = 3)]
pub struct UserV3 {
    pub level: u32,
    pub score: u16,
    pub name: String,
    #[ok_default(3)]
    pub tier: u8,
    #[ok_default("eu".to_string())]
    pub region: String,
}

fn main() {
    // 1) schema export
    let schema = okm_core::schema::TableSchema::of::<UserKey, UserV3>();
    println!("SCHEMA_JSON {}", serde_json::to_string(&schema).unwrap());

    // 2) Rust writes a row (payload + key bytes)
    let row = UserV3 { level: 9, score: 500, name: "alice".into(), tier: 2, region: "us".into() };
    let key = UserKey { org_id: 1, user_id: 2 };
    println!("PAYLOAD_HEX {}", row.encode_payload().iter().map(|b| format!("{b:02x}")).collect::<String>());
    println!("KEY_HEX {}", key.encode().iter().map(|b| format!("{b:02x}")).collect::<String>());

    // 3) decode Python-encoded bytes back into the typed row
    let ph = |h: &str| (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i+2], 16).unwrap()).collect::<Vec<u8>>();
    if let (Some(payload_hex), Some(key_hex)) = (std::env::args().nth(1), std::env::args().nth(2)) {
        let row2 = <UserV3 as okm_core::Document>::decode_payload(&ph(&payload_hex));
        let key2 = <UserKey as okm_core::KeyEncode>::decode(&ph(&key_hex));
        println!("RUST_DECODE level={} score={} name={} tier={} region={} org={} user={}",
            row2.level, row2.score, row2.name, row2.tier, row2.region, key2.org_id, key2.user_id);
        assert_eq!(row2.level, 4);
        assert_eq!(row2.score, 77);
        assert_eq!(row2.name, "bob");
        assert_eq!(row2.tier, 1);
        assert_eq!(row2.region, "asia");
        assert_eq!(key2.org_id, 7);
        assert_eq!(key2.user_id, 8);
    }
}
