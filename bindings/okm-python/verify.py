"""End-to-end verification of the okm Python binding: Rust derive writes,
Python reads through the same schema; Python encodes, Rust decodes."""
import json
import subprocess
import sys

TEST_RS = r'''
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
'''

PY = r'''
import okm

schema_json = SCHEMA_JSON
s = okm.Schema(schema_json)
assert s.layout_version == 3

# Rust wrote; Python reads.
payload = bytes.fromhex("PAYLOAD_HEX")
key = bytes.fromhex("KEY_HEX")
vals = s.decode_payload(payload)
assert vals["level"] == 9, vals
assert vals["score"] == 500, vals
assert vals["name"] == "alice", vals
assert vals["tier"] == 2, vals
assert vals["region"] == "us", vals
kd = s.decode_key(key)
assert kd["org_id"] == 1 and kd["user_id"] == 2, kd
print("python_decode ok:", vals, kd)

# Python writes (subset + explicit new fields); Rust decodes.
enc = s.encode_payload({"level": 9, "score": 500, "name": "alice", "tier": 2, "region": "us"})
assert enc == payload, (enc.hex(), payload.hex())
enc_key = s.encode_key({"org_id": 1, "user_id": 2})
assert enc_key == key
print("python_encode ok (byte-identical to the Rust derive)")
'''

# Step 1: build the Rust helper and get schema + rust bytes.
import tempfile, os, subprocess
d = tempfile.mkdtemp()
with open(os.path.join(d, "helper.rs"), "w") as f:
    f.write(TEST_RS)
r = subprocess.run(
    ["cargo", "run", "--features", "test-engines,schema-serde", "--example", "py_helper", "--", "x"],
    cwd="/home/master/world/okm", capture_output=True, text=True)
# Write it as an example instead.
os.makedirs("/home/master/world/okm/okm-core/examples", exist_ok=True)
with open("/home/master/world/okm/okm-core/examples/py_helper.rs", "w") as f:
    f.write(TEST_RS)
r = subprocess.run(
    ["cargo", "run", "-p", "okm-core", "--features", "test-engines,schema-serde",
     "--example", "py_helper"],
    cwd="/home/master/world/okm", capture_output=True, text=True)
if r.returncode != 0:
    print(r.stderr[-2000:])
    sys.exit(1)
lines = [l for l in r.stdout.splitlines() if " " in l]
data = dict(l.split(" ", 1) for l in lines if " " in l)
print("rust helper:", {k: (v[:40] + "..." if len(v) > 40 else v) for k, v in data.items()})

# Step 2: run the Python side.
py_src = PY.replace("SCHEMA_JSON", json.dumps(data["SCHEMA_JSON"])) \
           .replace("PAYLOAD_HEX", data["PAYLOAD_HEX"]) \
           .replace("KEY_HEX", data["KEY_HEX"])
r2 = subprocess.run([".venv/bin/python", "-c", py_src], cwd="/home/master/world/okm/bindings/okm-python",
                    capture_output=True, text=True)
print(r2.stdout)
if r2.returncode != 0:
    print(r2.stderr[-1500:])
    sys.exit(1)

# Step 3: Python-encoded variant (different values) decoded by Rust.
py_src2 = '''
import okm, json
s = okm.Schema(SCHEMA_JSON)
enc = s.encode_payload({"level": 4, "score": 77, "name": "bob", "tier": 1, "region": "asia"})
enc_key = s.encode_key({"org_id": 7, "user_id": 8})
print("PH2", enc.hex())
print("KH2", enc_key.hex())
'''.replace("SCHEMA_JSON", json.dumps(data["SCHEMA_JSON"]))
r3 = subprocess.run([".venv/bin/python", "-c", py_src2], cwd="/home/master/world/okm/bindings/okm-python",
                    capture_output=True, text=True)
ph2 = [l.split()[1] for l in r3.stdout.splitlines() if l.startswith("PH2")][0]
kh2 = [l.split()[1] for l in r3.stdout.splitlines() if l.startswith("KH2")][0]
r4 = subprocess.run(
    ["cargo", "run", "-p", "okm-core", "--features", "test-engines,schema-serde",
     "--example", "py_helper", "--", ph2, kh2],
    cwd="/home/master/world/okm", capture_output=True, text=True)
print([l for l in r4.stdout.splitlines() if "RUST_DECODE" in l])
if r4.returncode != 0:
    print(r4.stderr[-1500:])
    sys.exit(1)
print("ALL OK: Rust->Python and Python->Rust byte equality verified")
