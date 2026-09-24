"""okm_schema acceptance: the python DSL's assembled schema is byte-equal
to the Rust derive's `CollectionSchema::of` export for the same
declaration (the dynamic_cross_test User/UserKey shape), and the
declaration rules (index order, variable-width location, ns omission)
behave as specified. Run with `python3 tests/test_schema_dsl.py` (no
pytest dependency — the binding tests stay plain-script).
"""
import json
import os
import subprocess
import sys
import tempfile

import okm.okm_schema as okm_schema

# --- The mirror declaration: okm-core tests/dynamic_cross_test.rs shape ---

@okm_schema.KeyEncode
class UserKey:
    org_id: "u32"
    user_id: "u64"

@okm_schema.DocumentEncode
@okm_schema.ok_ref(UserKey)
@okm_schema.ok_ns(41)
@okm_schema.ok_layout(version=2)
@okm_schema.ok_index("by_level", fields=("level",))
class User:
    level: "u32"
    score: "u16"
    name: "str"

# --- 1. Byte-equality with the Rust derive (single source of truth) ---

RUST_RS = r'''
use okm_core::{KeyEncode, DocumentEncode};

#[derive(KeyEncode, Clone, Default)]
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode, Clone)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
#[ok_layout(version = 2)]
#[ok_index(by_level { fields(level) })]
pub struct User { pub level: u32, pub score: u16, pub name: String }

fn main() {
    let schema = okm_core::schema::CollectionSchema::of::<UserKey, User>();
    println!("{}", serde_json::to_string(&schema).unwrap());
}
'''

def rust_schema_json() -> str:
    with tempfile.TemporaryDirectory() as d:
        src = os.path.join(d, "src", "main.rs")
        toml = os.path.join(d, "Cargo.toml")
        os.makedirs(os.path.dirname(src), exist_ok=True)
        with open(src, "w") as f:
            f.write(RUST_RS)
        with open(toml, "w") as f:
            f.write(
                "[package]\nname='export'\nversion='0.1.0'\nedition='2021'\n"
                "[workspace]\n"
                "[dependencies.okm-core]\n"
                "path='" + os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "..", "okm-core")) + "'\n"
                "features=['schema-serde']\n"
                "[dependencies.serde_json]\nversion='1'\n"
            )
        r = subprocess.run(["cargo", "run", "--quiet"], cwd=d, capture_output=True, text=True)
        if r.returncode != 0:
            raise RuntimeError(r.stderr[-2000:])
        return r.stdout.strip().splitlines()[-1]


def test_schema_matches_rust_derive():
    rust = json.loads(rust_schema_json())
    entry = okm_schema.assemble(User)
    py = entry["schema"]
    assert py == rust, f"\npython: {json.dumps(py, indent=1)}\nrust:   {json.dumps(rust, indent=1)}"


def test_ns_recorded_beside_the_schema():
    entry = okm_schema.assemble(User)
    assert entry["ns"] == 41
    assert "ns" not in entry["schema"]


def test_module_assembly_and_omission():
    ns = {"User": User, "UserKey": UserKey, "unrelated": 42}
    block = okm_schema.assemble_module(ns)
    assert set(block["collections"].keys()) == {"User"}


# --- 2. Index order: source declaration order despite bottom-up application ---

@okm_schema.KeyEncode
class IdxKey:
    id: "u64"

@okm_schema.DocumentEncode
@okm_schema.ok_ref(IdxKey)
@okm_schema.ok_index("second", fields=("b",))
@okm_schema.ok_index("first", fields=("a",))
class Doc:
    a: "u32"
    b: "u32"

def test_index_slots_follow_source_order():
    entry = okm_schema.assemble(Doc)
    idx = entry["indexes"]
    assert [i["name"] for i in idx] == ["first", "second"]
    assert [i["slot"] for i in idx] == [okm_schema.INDEX_BASE, okm_schema.INDEX_BASE + 1]


# --- 3. Variable-width location rule ---

@okm_schema.KeyEncode
class BadKey:
    id: "u64"

@okm_schema.DocumentEncode
@okm_schema.ok_ref(BadKey)
@okm_schema.ok_index("by_name", fields=("name", "level"))
class Bad:
    name: "str"
    level: "u32"

def test_variable_width_must_be_last():
    try:
        okm_schema.assemble(Bad)
    except ValueError as e:
        assert "must be the last field" in str(e)
    else:
        raise AssertionError("expected a ValueError")


# --- 4. ns omission = actor-side auto allocation ---

@okm_schema.KeyEncode
class CKey:
    id: "u64"

@okm_schema.DocumentEncode
@okm_schema.ok_ref(CKey)
class Counter:
    count: "u64"

def test_ns_omitted_still_assembles():
    entry = okm_schema.assemble(Counter)
    assert "ns" not in entry
    assert entry["schema"]["key_len"] == 8


# --- 5. The assembled schema feeds okm.Schema (when the extension is built) ---

def test_schema_feeds_the_native_schema_class():
    try:
        import okm  # noqa: F401
    except ImportError:
        print("skip: okm extension not built (maturin develop)")
        return
    s = okm.Schema(json.dumps(okm_schema.assemble(Counter)["schema"]))
    key = s.encode_key({"id": 7})
    assert key == (7).to_bytes(8, "big")


if __name__ == "__main__":
    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for fn in fns:
        fn()
        print(f"ok {fn.__name__}")
    print(f"{len(fns)} passed")
