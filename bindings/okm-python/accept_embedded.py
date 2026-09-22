"""ADR-0022 embedded-mode acceptance: a schema declared Rust-side,
driven purely from Python, exhibits contract-conformant semantics —
the calling-discipline test (put/delete/overwrite against the
accumulator) plus func/partial index entry lifecycle. Run after
`maturin develop`:

    maturin develop && python3 accept_embedded.py
"""
import json
import subprocess
import sys

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
    """Export the schema from the Rust side (single source of truth)."""
    import tempfile, os
    with tempfile.TemporaryDirectory() as d:
        src = os.path.join(d, "src", "main.rs")
        toml = os.path.join(d, "Cargo.toml")
        os.makedirs(os.path.dirname(src), exist_ok=True)
        with open(src, "w") as f:
            f.write(SCHEMA_RS)
        with open(toml, "w") as f:
            f.write(
                "[package]\nname='export'\nversion='0.1.0'\nedition='2021'\n"
                "[workspace]\n"
                "[dependencies]\n"
                "okm-core={path='%s', features=['schema-serde']}\n"
                "okm-derive={path='%s'}\n"
                "serde_json='1'\n"
                % (os.path.abspath("../../okm-core"), os.path.abspath("../../okm-derive"))
            )
        out = subprocess.run(
            ["cargo", "run", "--quiet", "--manifest-path", toml],
            capture_output=True, text=True, timeout=300,
        )
        if out.returncode != 0:
            raise RuntimeError(out.stderr[-2000:])
        return out.stdout.strip()


class GroupCount(okm.ReduceLogic if hasattr(okm, "ReduceLogic") else object):
    """Mirror of the Rust-side ReduceLogic + ReduceCodec: Default pair:
    seed = Default::default() (8B BE zero), fold += 1, unfold -= 1."""
    pass


def make_logic():
    """The ReduceLogic subclass (defined at module level in real use;
    built here so the script runs against whatever base okm exposes)."""
    base = getattr(okm, "ReduceLogic", object)

    class GroupCount(base):
        def seed(self) -> bytes:
            return (0).to_bytes(8, "big")

        def fold(self, acc: bytearray, key: dict, doc: dict) -> None:
            n = int.from_bytes(bytes(acc), "big") + 1
            acc[:] = n.to_bytes(8, "big")

        def unfold(self, acc: bytearray, key: dict, doc: dict) -> None:
            n = int.from_bytes(bytes(acc), "big")
            if n == 0:
                raise ValueError("accumulator underflow: unfold without fold")
            acc[:] = (n - 1).to_bytes(8, "big")

    return GroupCount()


def doc(org, user, level, score, name):
    return {"org_id": org, "user_id": user, "level": level,
            "score": score, "name": name}


def key(org, user) -> bytes:
    return org.to_bytes(4, "big") + user.to_bytes(8, "big")


def main() -> int:
    schema = okm.Schema(schema_json())
    t = okm.Collection(schema, 42)

    # --- registration (binding-time, ADR-0022) ------------------------
    t.add_func_index(
        0x1002,
        lambda d: [(ord(c) & 0xFF).to_bytes(4, "big") for c in d["name"]],
    )
    t.add_partial_index(
        0x1003,
        ["level"],
        lambda d: d["name"] == "a",
        includes=["score"],
    )
    t.add_reduce(0x2001, ["level"], make_logic())

    # --- put folds ----------------------------------------------------
    t.put(key(1, 10), doc(1, 10, 4, 100, "a"))
    t.put(key(1, 11), doc(1, 11, 4, 200, "b"))
    acc = t.reduce_get({"level": 4})
    assert acc == (2).to_bytes(8, "big"), f"acc after two puts: {acc!r}"

    # --- overwrite: unfold old + fold new, no drift --------------------
    t.put(key(1, 10), doc(1, 10, 4, 100, "a2"))
    acc = t.reduce_get({"level": 4})
    assert acc == (2).to_bytes(8, "big"), f"overwrite drifted: {acc!r}"

    # --- overwrite across groups ---------------------------------------
    t.put(key(1, 11), doc(1, 11, 9, 200, "b"))
    assert t.reduce_get({"level": 4}) == (1).to_bytes(8, "big")
    assert t.reduce_get({"level": 9}) == (1).to_bytes(8, "big")

    # --- delete unfolds -------------------------------------------------
    t.delete(key(1, 11))
    assert t.reduce_get({"level": 9}) == (0).to_bytes(8, "big")

    # --- func index: fan-out entries + scan -----------------------------
    hits = t.scan(0x1002, (ord("a") & 0xFF).to_bytes(4, "big"))
    assert len(hits) == 1 and hits[0]["user_id"] == 10, hits
    # overwrite to a name without 'a': the entry sweeps
    t.put(key(1, 10), doc(1, 10, 4, 100, "zig"))
    assert t.scan(0x1002, (ord("a") & 0xFF).to_bytes(4, "big")) == []
    assert len(t.scan(0x1002, (ord("z") & 0xFF).to_bytes(4, "big"))) == 1

    # --- partial index: predicate gates entry existence ------------------
    # name "zig" is not admitted → no entry even though level=4 exists
    assert t.scan(0x1003, (4).to_bytes(4, "big")) == []
    t.put(key(1, 12), doc(1, 12, 4, 50, "a"))
    assert len(t.scan(0x1003, (4).to_bytes(4, "big"))) == 1
    t.delete(key(1, 12))
    assert t.scan(0x1003, (4).to_bytes(4, "big")) == []

    print("embedded-mode acceptance: ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
