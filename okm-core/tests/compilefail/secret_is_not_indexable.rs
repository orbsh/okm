use okm_core::{DocumentEncode, KeyEncode, Secret};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

// ADR-0069 §3: an index entry's KEY carries the field's encoded value, so an
// index over a secret is a sorted copy of the secret. The declaration is
// refused at expansion — the secret has no way to be indexed at all.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(214)]
#[ok_index(by_token { fields(token) })]
pub struct D {
    pub token: Secret,
}

fn main() {}