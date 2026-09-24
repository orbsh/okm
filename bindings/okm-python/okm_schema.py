"""OKM python schema-declaration DSL (ADR-0026 §4): the python mirror of
the Rust derive. Classes with type annotations declare collections; the
decorators carry the declaration metadata the derive reads from
`#[ok_*]` attributes. Assembling a class produces the serde form of
`okm_core::schema::CollectionSchema` — the exact object `okm.Schema.from_json`
consumes and what aura's `StorePlan::from_schema` parses.

Rules locked to the Rust derive (okm-derive `parse_schema`/`field_encoders`):

- Declaration order is wire order. Fixed-width kinds are HOT payload fields
  (contiguous offsets from 0); width-0 kinds (str/bytes) are COLD TLV
  frames with tag = declaration index. Key fields are always fixed width.
- i64 shares the u64 wire (two's complement IS the BE encoding) — kind U64.
- Payload header `[version u8][hot_len u16 BE]` = 3 bytes; layout_version
  defaults to 1.
- Slot map is the ADR-0016 constant allocation: slot = 4-bit segment
  (high nibble) + 12-bit counter. primary=0, dynamic=1, dict_id=2,
  dict_name=3, index base = segment 1 counter 1, reduce base = segment 2
  counter 1, junction base = segment 3.
- Index slots follow SOURCE declaration order (the Rust derive reads
  `#[ok_index]` attributes top-to-bottom). With factory-style decorators
  the resulting stamp list already matches that order (verified) — no
  reversal.
- A variable-width field may appear at most once in an index
  fields/includes list and must be LAST (no static width after it).
- ns is NOT part of CollectionSchema (it binds at plan construction):
  `@ok_ns(N)` records a number for standalone embedded use; omitting it
  is the actor-side path — the host injects the registry-allocated ns.
"""
from __future__ import annotations

# Slot map constants (okm-core model/index.rs).
PRIMARY = 0
DYNAMIC = 1
DICT_ID = 2
DICT_NAME = 3
INDEX_BASE = (1 << 12) | 1
REDUCE_BASE = (2 << 12) | 1
JUNCTION_BASE = 3 << 12

# Wire types: fixed-width (name, kind, byte width) plus the variable-length
# cold kinds. `int`/`int64` spell u64 (two's complement = the BE wire).
_PRIMS = {
    "u8": ("U8", 1),
    "u16": ("U16", 2),
    "u32": ("U32", 4),
    "u64": ("U64", 8),
    "int8": ("U8", 1),
    "int16": ("U16", 2),
    "int32": ("U32", 4),
    "int64": ("U64", 8),
    "int": ("U64", 8),
    "str": ("Str", 0),
    "bytes": ("Bytes", 0),
}


def _annotations(cls) -> list[tuple[str, str]]:
    """The class's own annotations as (name, type-name) pairs.

    Values may be strings (the module's wire-type placeholders) or type
    objects (`str`, `int`) — read the type name uniformly.
    """
    try:
        from typing import get_type_hints
        hints: dict = get_type_hints(cls)
    except Exception:
        hints: dict = dict(getattr(cls, "__annotations__", {}))
    out = []
    for name, value in hints.items():
        if isinstance(value, str):
            out.append((name, value))
        else:
            out.append((name, getattr(value, "__name__", str(value))))
    return out


def _prim(ty: str, ctx: str) -> tuple[str, int]:
    got = _PRIMS.get(ty)
    if got is None:
        raise ValueError(
            f"{ctx}: unsupported type `{ty}` "
            "(scalars: u8/u16/u32/u64/int; variable-length: str/bytes)"
        )
    return got


def _field(name: str, ty: str, width: int, offset: int, tag: int | None) -> dict:
    return {
        "name": name,
        "ty": ty,
        "width": width,
        "offset": offset,
        "tag": tag,
        "default": None,
        "expect_len": None,
    }


class _Decl:
    """Payload accumulator one decorator factory stamps onto the class."""

    def __init__(self, payload: dict):
        self.payload = payload


def _stamp(attr: str, payload: dict):
    """The decorator-factory body: capture the payload, return an identity
    decorator that appends it to the class's `_okm_<attr>` list."""

    def apply(cls):
        lst = getattr(cls, f"_okm_{attr}", None)
        if lst is None:
            lst = []
            setattr(cls, f"_okm_{attr}", lst)
        lst.append(payload)
        if attr == "ref":
            # Keep the key class itself for annotation readback.
            cls._okm_ref_class = payload[0]
        return cls

    return apply


def ok_ref(key_cls):
    """`@ok_ref(CounterKey)` — pin the key class (identity struct)."""
    return _stamp("ref", {0: key_cls})


def ok_ns(ns: int):
    """`@ok_ns(41)` — the collection's ns segment, for STANDALONE embedded
    use (the number must be given; there is no compile-time registry).
    Omitted on an aura actor: the host injects the registry-allocated ns
    into the plan. Never serialized into CollectionSchema."""

    def apply(cls):
        cls._okm_ns = ns
        return cls

    return apply


def ok_layout(version: int):
    """`@ok_layout(version=2)` — document header layout version."""

    def apply(cls):
        cls._okm_layout = version
        return cls

    return apply


def ok_index(*args, fields: tuple = (), includes: tuple = ()):
    """`@ok_index("by_org", fields=("org_id", "created_at"), includes=("bio_len",))`.

    The name may also ride the `name=` kwarg. Slots follow SOURCE
    declaration order (assigned at assembly, after the bottom-up
    application list is reversed).
    """
    name = args[0] if args else None
    if not isinstance(name, str):
        raise ValueError("ok_index: first argument must be the index name")
    return _stamp(
        "index",
        {
            "name": name,
            "fields": list(fields),
            "includes": list(includes),
            "kind": "plain",
        },
    )


def KeyEncode(cls):
    """`@KeyEncode` — mark the class as a key (identity) struct. The
    annotations are read by the document's assembler."""
    cls._okm_role = "key"
    return cls


def DocumentEncode(cls):
    """`@DocumentEncode` — mark the class as a document (collection
    declaration point)."""
    cls._okm_role = "document"
    return cls


def _validate_index(index: dict, index_name: str, field_widths: dict):
    """Location rules (mirrors the derive's compile-time checks): every
    field must be declared; a variable-width field has no static width, so
    it may appear at most once per list and must be LAST."""
    for kw, names in (("fields", index["fields"]), ("includes", index["includes"])):
        for pos, fname in enumerate(names):
            if fname not in field_widths:
                raise ValueError(
                    f"ok_index[{index_name}]: field `{fname}` is not a declared field"
                )
            if field_widths[fname] == 0 and pos != len(names) - 1:
                raise ValueError(
                    f"ok_index[{index_name}]: variable-length field `{fname}` "
                    f"must be the last field of {kw}"
                )


def assemble(document_cls) -> dict:
    """Assemble one `@DocumentEncode` class → the collection entry:
    `{"schema": <CollectionSchema serde>, "ns"?: N, "indexes"?: [...]}`.

    The schema is the exact serde form of
    `okm_core::schema::CollectionSchema` (all seven fields — the serde has
    no defaults).
    """
    role = getattr(document_cls, "_okm_role", None)
    if role != "document":
        raise ValueError(f"{document_cls.__name__}: missing @DocumentEncode")
    refs = getattr(document_cls, "_okm_ref", None)
    if not refs:
        raise ValueError(
            f"{document_cls.__name__}: missing @ok_ref(KeyClass)"
        )
    key_cls = document_cls._okm_ref_class

    # --- Key fields: declaration order, contiguous offsets, fixed width ---
    key_fields = []
    key_names = []
    key_len = 0
    for fname, fty in _annotations(key_cls):
        kind, width = _prim(fty, f"key field `{fname}`")
        if width == 0:
            raise ValueError(
                f"key field `{fname}`: variable-length kinds are payload-only"
            )
        key_fields.append(_field(fname, kind, width, key_len, None))
        key_names.append(fname)
        key_len += width

    # --- Payload fields: hot (fixed width) / cold (variable TLV) ---
    hot_fields = []
    cold_fields = []
    hot_width = 0
    widths: dict[str, int] = {}
    for decl_index, (fname, fty) in enumerate(_annotations(document_cls)):
        kind, width = _prim(fty, f"payload field `{fname}`")
        widths[fname] = width
        if width > 0:
            hot_fields.append(_field(fname, kind, width, hot_width, None))
            hot_width += width
        else:
            cold_fields.append(_field(fname, kind, 0, 0, decl_index))

    layout_version = getattr(document_cls, "_okm_layout", 1)

    # --- Indexes: the stamp list arrives in SOURCE declaration order
    # (verified: with factory-style decorators the bottom line's stamper
    # runs first, and that line is the first @ok_index in the source).
    # Slots follow: INDEX-segment counter from the declaration base.
    index_decls = list(getattr(document_cls, "_okm_index", []))
    indexes = []
    for i, index in enumerate(index_decls):
        _validate_index(index, index["name"], widths)
        indexes.append(
            {
                "name": index["name"],
                "slot": INDEX_BASE + i,
                "fields": index["fields"],
                "includes": index["includes"],
                "kind": index["kind"],
            }
        )

    schema = {
        "key_len": key_len,
        "key_fields": key_fields,
        "layout_version": layout_version,
        "hot_width": hot_width,
        "payload_header_len": 3,  # [version u8][hot_len u16 BE]
        "hot_fields": hot_fields,
        "cold_fields": cold_fields,
        "slots": {
            "primary": PRIMARY,
            "dynamic": DYNAMIC,
            "dict_id": DICT_ID,
            "dict_name": DICT_NAME,
            "declared_index_base": INDEX_BASE,
            "declared_reduce_base": REDUCE_BASE,
            "junction_base": JUNCTION_BASE,
        },
    }

    entry = {"schema": schema}
    ns = getattr(document_cls, "_okm_ns", None)
    if ns is not None:
        # Beside the schema, never inside: ns binds at plan construction.
        entry["ns"] = ns
    if indexes:
        entry["indexes"] = indexes
    return entry


def assemble_module(module_globals: dict) -> dict:
    """Assemble every `@DocumentEncode` class in a namespace → the
    interface_schema `storage` block:
    `{"collections": {name: <entry>, ...}}`.

    Empty (no declared classes) — the caller decides whether an empty
    block means anything (aura: an explicit-only storage declaration
    still counts; an empty one is omitted so it cannot shadow it).
    """
    collections = {}
    for name, member in module_globals.items():
        if name.startswith("_"):
            continue
        if getattr(member, "_okm_role", None) != "document":
            continue
        collections[name] = assemble(member)
    return {"collections": collections}
