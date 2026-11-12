# ADR-0063: The document map is a type

Date: 2026-11-12
Status: RULED & LANDED 2026-11-12

**Depends on:** ADR-0012 (the object model: a map IS an n-TLV list), ADR-0035
(the whole-document API: `fetch` answers a `Dict`), ADR-0057 (the dynamic
segment's block table, which writes a map's insertion order)

## 1. Context

A document's name → value map used to be spelled `BTreeMap<String, Variant>`
inside the public enum variant: `Variant::Map(BTreeMap<String, Variant>)`. That
made the container the API. Two consequences followed, and neither was
negotiable independently of the other.

**Ordering was name-sorted** because `BTreeMap` sorts. A document is assembled
in the order its names were inserted — a decoded row's declared fields, then the
dynamic names as they appeared on the wire — and that order is provenance: it is
how the document was built. Sorting it away made every caller who cared about
which name came first re-derive the order from somewhere else, and nothing else
was recorded.

**The container could not change.** `Variant::Map` names its payload type, so
every caller that writes `match value { Variant::Map(m) => ... }` or takes
`&BTreeMap<String, Variant>` in a signature had baked the container into its own
API. Swapping the map later — for an insertion-ordered one, a small-map
optimization, anything — would have been a breaking change across every crate
and every downstream caller.

## 2. Decision: a type, surfaced through an explicit method list

The map is a type: `pub struct Dict(IndexMap<String, Variant>)`, private field,
in `okm-value`. `Variant::Map(Dict)` is the only spelling. The variant names the
abstraction (an object: names to values); the type names the thing that carries
it, and it owns the contract.

The surface is an explicit, hand-written method list, each forwarding to the
backing map: `new`, `with_capacity`, `insert`, `get`, `get_mut`, `contains_key`,
`remove`, `len`, `is_empty`, `iter`, `keys`, plus the trait impls a method
cannot carry (`Clone`/`Debug`/`Default`/`PartialEq`, `From<IndexMap<..>>`,
`From<[(String, Variant); N]>`, `FromIterator`, `Extend`, `IntoIterator` for
`Dict`, `&Dict` and `&mut Dict`). What that list does not name does not exist.
The list was derived from real call sites, not from imagination: `Deref` was
removed, the workspace was compiled, and each missing method added as the
compiler asked for it. `clear`, `values`, `iter_mut` and `entry` had no caller
and are therefore not on it.

Insertion order is the contract: an overwrite keeps a name's place, a new name
lands last, `remove` is the SHIFT form (`shift_remove`) so a removal closes the
gap instead of moving the last name into the hole, and nothing sorts. Equality
stays order-insensitive because a document is a set of name → value pairs;
insertion order is provenance, not identity.

## 3. Rejected

**Keeping the alias (`pub type Dict = ...`).** An alias is not a boundary: it is
another name for the container, so the container is still in every signature and
its method set is still reachable, including the ones that reorder. The type has
to be a type for the boundary to exist at all.

**Exposing `IndexMap` in the variant, and documenting "don't rely on it".**
`Variant::Map(IndexMap<String, Variant>)` freezes the container into the public
API the moment it is written — by the same argument as §1. A doc comment cannot
undo a type that is already in a caller's `match` arm.

**`Deref`/`DerefMut` to the backing map.** This is the shape the type had
before this ruling, and it was rejected for three reasons:

1. **Coercion is an outward leak.** If `Dict` derefs to its container, any
   function anywhere that takes `&Container` accepts `&Dict`. An external
   signature then names our backing map — the exact thing the type exists to
   prevent — and a caller is free to write `fn takes(&IndexMap<..>)` and pass a
   `Dict` to it. The container is back in the API, through the back door.
2. **The whole method set is exposed.** Deref exposes everything the container
   has: `swap_remove`, `sort_keys`, `swap_indices`, and the rest. Any of them
   reorders or rewrites the map in a way that breaks the promise the type
   exists to keep. A guarantee whose violation is one method call away is not a
   guarantee.
3. **Deref cannot hide part of a surface.** There is no way to deref to "the
   map, but only these eleven methods". Partial exposure is exactly what is
   wanted here, and deref is all-or-nothing by construction.

## 4. Consequences

**Swapping the container is one impl block.** Every method in §2 forwards to
`self.0`; changing `IndexMap` for something else edits those bodies plus the two
conversion impls that must name the incoming type (`From<..>` and the
`IntoIterator` associated types), and nothing else. No caller's code changes —
the leak being closed is exactly the case where it would have had to.

**The cost is one screen of forwarding boilerplate.** That is the honest price
of the guarantee: the alternative was a smaller file and a contract that could
not be stated. The file is written so the whole contract reads in one screen.

**Stored bytes are unaffected.** Encodings write a document's schema
declaration order and a dynamic segment's block table follows the map's own
order (ADR-0012, ADR-0057); none of them consult the container's ordering
beyond what the type already promises. Equality is unchanged (order-insensitive
`PartialEq` over name → value pairs).

## 5. The deliberate asymmetry with `Variant::Array(Vec<Variant>)`

`Array` holds a plain `Vec<Variant>`, and that stays. A `Vec` is the canonical
sequence: there is one implementation, its behaviour is fully specified by the
standard library, and naming it costs the model nothing — no design choice is
being hidden. An ordered name → value map is not that: sorted, hashed,
insertion-ordered and memory-per-name all differ, the choice is observable
through iteration order, and it is the one thing a reader of a document can see.
So the sequence is exposed and the map is wrapped. The rule is not "wrap
everything" or "wrap nothing"; it is that a type earns a wrapper when it owns a
choice, and a map's ordering is a choice while a vector's sequence is not.