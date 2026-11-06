use okm_core::{DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub channel: u64,
    pub member: u64,
}

// A lead face's `fields(...)` names KEY fields. `nickname` is a payload field,
// so the declaration is refused — at the declaration, not at the first probe.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(212)]
#[ok_lead(by_nickname { fields(nickname) })]
pub struct D {
    pub nickname: String,
}

fn main() {}