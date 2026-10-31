"""ADR-0022 embedded-mode acceptance: a schema declared Rust-side,
driven purely from Python, exhibits contract-conformant semantics —
the calling-discipline test (put/delete/overwrite against the
accumulator) plus func/partial index entry lifecycle. Run after
`maturin develop`:

    maturin develop && python3 accept_embedded.py
"""
import json
import os
import subprocess
import sys

import okm

# The acceptance runs from anywhere — including a venv holding the WHEEL rather than
# this checkout. The schema exporter below builds a throwaway crate against this repo,
# so those paths come from this file's location, never from the cwd (`../../okm-core`
# resolved against a venv's cwd is a missing directory, not a schema).
HERE = os.path.dirname(os.path.abspath(__file__))

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
                % (os.path.join(HERE, "../../okm-core"), os.path.join(HERE, "../../okm-derive"))
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
    # Provenance first: a `okm/*.so` left in this directory by a hand-copied build
    # shadows an installed wheel silently — which is how a wheel check can "pass"
    # against yesterday's code. Run with `python -P` to test the wheel, and read this
    # line in the log to know which module answered.
    print(f"okm: {okm.__file__}")
    schema = okm.Schema(schema_json())
    t = okm.Collection(schema, 42)

    # --- table meta (ADR-0043): one stamp, whichever mode declared it ---
    # The dynamic table stamps the same `[ns][0x0F]` row the typed path does,
    # with `schema.layout_version`; a second call answers the stored row.
    assert t.meta() is None, "a fresh store carries no record — the read verb says so"
    meta = t.ensure_layout()
    # ADR-0048: a fresh store carries BOTH names — the payload shape and the
    # fingerprint of the faces the schema declares. This schema was exported from
    # a Rust declaration, which is why it can name them at all.
    assert meta["version"] == bytes([schema.layout_version]), f"meta: {meta!r}"
    assert len(meta["faces"]) == 16, f"faces: {meta['faces']!r}"
    assert t.meta() == meta, "the read verb answers the row the stamp wrote"
    assert t.ensure_layout() == meta, "a stamped store answers the same row"

    # A caller's own name rides beside the runtime's, and cannot edit the stamp
    # away: `set_meta` re-stamps `version` after the caller's write.
    after = t.set_meta("build", b"20261031")
    assert after == dict(meta, build=b"20261031"), f"set_meta: {after!r}"
    assert t.meta() == after, "the name landed in the store, not in a cache"
    assert t.set_meta("version", bytes([9]))["version"] == meta["version"], (
        "the runtime re-stamps version — a caller cannot edit the shape away"
    )

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
    t.merge(key(1, 10), doc(1, 10, 4, 100, "a"))
    t.merge(key(1, 11), doc(1, 11, 4, 200, "b"))
    acc = t.reduce_get({"level": 4})
    assert acc == (2).to_bytes(8, "big"), f"acc after two puts: {acc!r}"

    # --- overwrite: unfold old + fold new, no drift --------------------
    t.merge(key(1, 10), doc(1, 10, 4, 100, "a2"))
    acc = t.reduce_get({"level": 4})
    assert acc == (2).to_bytes(8, "big"), f"overwrite drifted: {acc!r}"

    # --- overwrite across groups ---------------------------------------
    t.merge(key(1, 11), doc(1, 11, 9, 200, "b"))
    assert t.reduce_get({"level": 4}) == (1).to_bytes(8, "big")
    assert t.reduce_get({"level": 9}) == (1).to_bytes(8, "big")

    # --- delete unfolds -------------------------------------------------
    t.purge(key(1, 11))
    assert t.reduce_get({"level": 9}) == (0).to_bytes(8, "big")

    # --- func index: fan-out entries + scan -----------------------------
    hits = t.scan(0x1002, (ord("a") & 0xFF).to_bytes(4, "big"))
    assert len(hits) == 1 and hits[0]["user_id"] == 10, hits
    # overwrite to a name without 'a': the entry sweeps
    t.merge(key(1, 10), doc(1, 10, 4, 100, "zig"))
    assert t.scan(0x1002, (ord("a") & 0xFF).to_bytes(4, "big")) == []
    assert len(t.scan(0x1002, (ord("z") & 0xFF).to_bytes(4, "big"))) == 1

    # --- exact scan: a probe completed by the key tail is dropped --------
    # The fan-out value is 4 bytes and the key tail follows with no boundary
    # byte, so the 5-byte probe (value + the key's first byte — org 1's high
    # byte is 0x00) is satisfied by that entry's BYTES. It is not a match on
    # the value: the stored value is shorter than the probe, which neither
    # variant accepts. scan_exact additionally requires the probe to consume
    # the whole field segment.
    vz = (ord("z") & 0xFF).to_bytes(4, "big")
    assert t.scan(0x1002, vz + b"\x00") == [], "spill is no prefix hit"
    assert t.scan_exact(0x1002, vz + b"\x00") == [], "spill is no exact hit"
    assert len(t.scan(0x1002, vz)) == 1, "the value itself is a prefix hit"
    assert len(t.scan_exact(0x1002, vz)) == 1, "and an exact hit"

    # --- partial index: predicate gates entry existence ------------------
    # name "zig" is not admitted → no entry even though level=4 exists
    assert t.scan(0x1003, (4).to_bytes(4, "big")) == []
    t.merge(key(1, 12), doc(1, 12, 4, 50, "a"))
    assert len(t.scan(0x1003, (4).to_bytes(4, "big"))) == 1
    t.purge(key(1, 12))
    assert t.scan(0x1003, (4).to_bytes(4, "big")) == []

    # --- path index (ADR-0029): a face over the schema-free segment ------
    # `note` is NOT declared, so `merge` puts it in the dynamic segment — the only
    # place a path index earns its keep (a declared field's depth-1 path is what a
    # Plain index already covers). Declaring it before any write proves the face
    # starts empty and fills from the documents.
    t.add_path_index(0x1004, "note", "str")
    assert t.scan(0x1004, b"hot") == [], "a declared face starts empty"
    t.merge(key(1, 20), {"note": "hot"})
    hits = t.scan(0x1004, b"hot")
    assert len(hits) == 1 and hits[0]["user_id"] == 20, hits
    # overwrite with a different value sweeps the old entry (the entry key encodes
    # the value, so the sweep is the only thing that can reach it)
    t.merge(key(1, 20), {"note": "cold"})
    assert t.scan(0x1004, b"hot") == [], "the old entry is gone"
    assert len(t.scan(0x1004, b"cold")) == 1

    # Array fan-out: one entry per resolved value, positioned by the index.
    t.add_path_index(0x1005, "tags[1]", "str")
    t.merge(key(1, 21), {"tags": ["x", "y"]})
    assert len(t.scan(0x1005, b"y")) == 1, "the pinned position resolved"
    assert t.scan(0x1005, b"x") == [], "position 1 is not position 0"

    # A value type the entry encoder cannot produce is refused at declaration,
    # rather than declared as a face that would answer nothing forever.
    try:
        t.add_path_index(0x1006, "note", "uuid4")
    except ValueError as e:
        assert "u8/u16/u32/u64/str/fixed" in str(e), e
    else:
        raise AssertionError("an unencodable value type must be refused")

    # --- faces_status + rebuild (ADR-0048) --------------------------------
    # The store was stamped when the table was opened; every access method above was
    # declared AFTER that, so the declaration the stamp describes is not the one the
    # table now carries. That is `stale` — and it is the honest answer, because those
    # faces have never been derived from these documents.
    before = t.faces_status()
    assert before["state"] == "stale", before
    assert before["stored"] is not None

    # A closure cannot be hashed, so a swapped derive is invisible to the
    # fingerprint unless the caller says it changed: the version rides the number.
    t.add_func_index(0x1007, lambda d: [b"v2"], version=2)
    after = t.faces_status()
    assert after["state"] == "stale", after
    assert after["current"] != before["current"], "the version rode the fingerprint"

    # The rebuild is the repair: sweep every declared face, re-derive from the
    # documents, stamp last. After it the table agrees with its own declaration.
    report = t.rebuild_faces()
    assert report["faces_swept"] >= 4, report
    assert t.faces_status()["state"] == "match", t.faces_status()
    # The re-derived faces answer: the path index was declared before the writes.
    assert len(t.scan(0x1004, b"cold")) == 1

    print("embedded-mode acceptance: ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
