"""ADR-0022 remote-mode acceptance (acceptance 2): Python-planned wire
frames must be byte-identical to Rust-planned ones for the same scenario.

Run: python3 accept_remote.py   (after `maturin build` + install; compiles
accept_remote_rust.rs as a helper binary, then compares hex-for-hex).
"""
import json
import os
import subprocess
import sys
import tempfile

import okm

SCHEMA_RS = r'''
use okm_core::{KeyEncode, DocumentEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(42)]
#[ok_layout(version = 2)]
pub struct User {
    pub level: u32,
    pub score: u16,
    pub name: String,
}

fn main() {
    let schema = okm_core::schema::CollectionSchema::of::<UserKey, User>();
    println!("{}", serde_json::to_string(&schema).unwrap());
}
'''


def schema_json() -> str:
    import tempfile
    with tempfile.TemporaryDirectory() as d:
        os.makedirs(os.path.join(d, "src"))
        open(os.path.join(d, "src", "main.rs"), "w").write(SCHEMA_RS)
        open(os.path.join(d, "Cargo.toml"), "w").write(
            "[package]\nname='export'\nversion='0.1.0'\nedition='2021'\n[workspace]\n"
            "[dependencies]\n"
            "okm-core={path='%s', features=['schema-serde']}\n"
            "okm-derive={path='%s'}\nserde_json='1'\n"
            % (os.path.abspath("../../okm-core"), os.path.abspath("../../okm-derive"))
        )
        out = subprocess.run(
            ["cargo", "run", "--quiet", "--manifest-path", os.path.join(d, "Cargo.toml")],
            capture_output=True, text=True, timeout=300,
        )
        if out.returncode != 0:
            raise RuntimeError(out.stderr[-2000:])
        return out.stdout.strip()


def doc(org, user, level, score, name):
    return {"org_id": org, "user_id": user, "level": level,
            "score": score, "name": name}


def key(org, user) -> bytes:
    return org.to_bytes(4, "big") + user.to_bytes(8, "big")


def make_logic():
    base = getattr(okm, "ReduceLogic", object)

    class GroupCount(base):
        """Mirror of the Rust-side GroupCount: u64 BE, fold += 1."""
        def seed(self) -> bytes:
            return (0).to_bytes(8, "big")

        def fold(self, acc: bytearray, key: dict, doc: dict) -> None:
            n = int.from_bytes(bytes(acc), "big") + 1
            acc[:] = n.to_bytes(8, "big")

        def unfold(self, acc: bytearray, key: dict, doc: dict) -> None:
            n = int.from_bytes(bytes(acc), "big")
            if n == 0:
                raise ValueError("underflow")
            acc[:] = (n - 1).to_bytes(8, "big")

    return GroupCount()


def run_rust_helper() -> dict:
    """Compile + run accept_remote_rust.rs; parse FRAME_/ACC_ lines."""
    src = open("accept_remote_rust.rs").read()
    with tempfile.TemporaryDirectory() as d:
        os.makedirs(os.path.join(d, "src"))
        open(os.path.join(d, "src", "main.rs"), "w").write(src)
        open(os.path.join(d, "Cargo.toml"), "w").write(
            "[package]\nname='accept_remote'\nversion='0.1.0'\nedition='2021'\n[workspace]\n"
            "[dependencies]\n"
            "okm-core={path='%s', features=['schema-serde','test-engines']}\n"
            "okm-derive={path='%s'}\n"
            "okm-dynamic={path='%s'}\n"
            "okm-wire={path='%s'}\n"
            % tuple(os.path.abspath("../../" + p)
                    for p in ("okm-core", "okm-derive", "okm-dynamic", "okm-wire"))
        )
        out = subprocess.run(
            ["cargo", "run", "--quiet", "--manifest-path", os.path.join(d, "Cargo.toml")],
            capture_output=True, text=True, timeout=600,
        )
        if out.returncode != 0:
            raise RuntimeError(out.stderr[-2000:])
    frames, accs = {}, {}
    for line in out.stdout.strip().splitlines():
        parts = line.split()
        if parts[0].startswith("FRAME_"):
            frames[parts[0][6:]] = parts[1]
        elif parts[0].startswith("ACC_"):
            step = parts[0][4:]
            accs.setdefault(step, {})[bytes.fromhex(parts[0].split("_", 2)[2])] = bytes.fromhex(parts[1])
    return {"frames": frames, "accs": accs}


def main() -> int:
    schema = okm.Schema(schema_json())
    rust = run_rust_helper()
    t = okm.Collection(schema, 42)
    t.add_func_index(
        0x1002,
        lambda d: [(ord(c) & 0xFF).to_bytes(4, "big") for c in d["name"]],
    )
    t.add_partial_index(0x1003, ["level"], lambda d: d["name"] == "a")
    t.add_reduce(0x2001, ["level"], make_logic())

    accs_cache: dict = {}
    frames = {}

    frame, new_accs = t.plan_put(key(1, 10), doc(1, 10, 4, 100, "a"), accs=accs_cache)
    accs_cache.update(dict(new_accs))
    frames["PUT1"] = frame.hex()

    frame, new_accs = t.plan_put(key(1, 10), doc(1, 10, 4, 100, "a2"),
                                 old=doc(1, 10, 4, 100, "a"), accs=accs_cache)
    accs_cache.update(dict(new_accs))
    frames["PUT2"] = frame.hex()

    frame, new_accs = t.plan_put(key(1, 11), doc(1, 11, 4, 200, "b"),
                                 old=doc(1, 11, 9, 200, "b"), accs=accs_cache)
    accs_cache.update(dict(new_accs))
    frames["PUT3"] = frame.hex()

    frame, new_accs = t.plan_delete(key(1, 10), old=doc(1, 10, 4, 100, "a2"),
                                    accs=accs_cache)
    accs_cache.update(dict(new_accs))
    frames["DEL1"] = frame.hex()

    ok = True
    for step in ("PUT1", "PUT2", "PUT3", "DEL1"):
        if frames[step] != rust["frames"][step]:
            print(f"{step}: MISMATCH")
            print(f"  python: {frames[step]}")
            print(f"  rust:   {rust['frames'][step]}")
            ok = False
        else:
            print(f"{step}: identical ({len(frames[step]) // 2} bytes)")
    print("remote-mode byte equality: ALL PASS" if ok else "remote-mode: FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
