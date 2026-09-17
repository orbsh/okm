//! `Enum<T>` — C-like enum fields carried as a single `u8` tag.

/// The wire contract for enum-typed fields: the implementor fixes each
/// variant's byte value. Tags are **explicit, not positional** — the wire
/// format is a persistent contract and must not shift when variants are
/// inserted or reordered; declare tags once and keep them stable.
///
/// ```ignore
/// #[derive(Clone, Copy, PartialEq, Debug)]
/// enum State { Active, Suspended, Closed }
/// impl EnumTag for State {
///     const TAGS: &[(Self, u8)] = &[(State::Active, 0), (State::Suspended, 1), (State::Closed, 9)];
/// }
/// ```
/// Free function so generated code can call it without naming `T`
/// (inference resolves from the call site's context).
pub fn enum_from_name<T: EnumTag>(name: &str) -> Option<T> {
    T::from_name(name)
}

pub trait EnumTag: Copy + Sized + PartialEq + std::fmt::Debug + 'static {
    /// The full variant → tag table. Every variant must appear exactly
    /// once; tags need not be contiguous (gaps reserve room).
    const TAGS: &'static [(Self, u8)];

    fn tag(self) -> u8 {
        Self::TAGS
            .iter()
            .find(|(v, _)| *v == self)
            .map(|(_, t)| *t)
            .unwrap_or_else(|| panic!("EnumTag: variant missing from TAGS table: {self:?}"))
    }
    /// Inverse of [`EnumTag::tag`]; panics on an unknown tag (a wire
    /// violation, not a normal error path).
    fn from_tag(t: u8) -> Self {
        Self::TAGS
            .iter()
            .find(|(_, tg)| *tg == t)
            .map(|(v, _)| *v)
            .unwrap_or_else(|| panic!("EnumTag: unknown tag {t}"))
    }
    /// Variant name for the map view (ADR-0012 row-map bridge):
    /// `DynamicValue::Str(name)`. Derived from the TAGS table via Debug
    /// — the variant's short name is the text after `::`.
    fn name(&self) -> String {
        let dbg = format!("{self:?}");
        dbg.rsplit("::").next().unwrap_or(&dbg).to_owned()
    }
    /// Inverse of [`Self::name`]: map view → variant. Unknown names are
    /// normal dynamic-reader input — `None`, never panic.
    fn from_name(name: &str) -> Option<Self> {
        Self::TAGS
            .iter()
            .find(|(v, _)| v.name() == name)
            .map(|(v, _)| *v)
    }
}

/// Newtype the derive macros recognize in field position (`Enum<State>`).
/// Wire is one byte — width 1, fixed, at every destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Enum<T: EnumTag>(pub T);

impl<T: EnumTag> Enum<T> {
    /// Bridge constructor (ADR-0012 row-map bridge): `T` inferred from
    /// the field type — no type interpolation in generated code.
    pub fn from_dyn(v: T) -> Self {
        Enum(v)
    }
}

impl<T: EnumTag> Default for Enum<T> {
    /// Tag 0 — the schema-evolution default (same rule as decode filling
    /// missing tail fields). Explicit tags make this a contract: the first
    /// declared variant must own tag 0.
    fn default() -> Self {
        Enum(T::from_tag(0))
    }
}

impl<T: EnumTag> Enum<T> {
    pub fn encode(&self) -> Vec<u8> {
        vec![self.0.tag()]
    }
    /// Inverse of [`Enum::encode`]; `b` is exactly 1 byte.
    pub fn decode(b: &[u8]) -> Self {
        Enum(T::from_tag(b[0]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum State {
        Active,
        Suspended,
        Closed,
    }

    impl EnumTag for State {
        const TAGS: &'static [(Self, u8)] = &[
            (State::Active, 0),
            (State::Suspended, 1),
            (State::Closed, 9),
        ];
    }

    #[test]
    fn tag_round_trip_with_gaps() {
        for s in [State::Active, State::Suspended, State::Closed] {
            assert_eq!(Enum::<State>::decode(&Enum(s).encode()).0, s);
        }
        // Non-contiguous tags survive verbatim: 9 stays 9, no renumbering.
        assert_eq!(Enum(State::Closed).encode(), vec![9]);
    }

    #[test]
    #[should_panic(expected = "unknown tag")]
    fn unknown_tag_panics() {
        let _ = State::from_tag(2);
    }
}
