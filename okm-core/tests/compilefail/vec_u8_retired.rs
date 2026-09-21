use okm_core::{DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone)]
pub struct DocKey {
    pub id: u64,
}

#[derive(DocumentEncode)]
#[ok_ref(DocKey)]
#[ok_ns(2)]
pub struct Doc {
    pub blob: Vec<u8>,
}

fn main() {}
