use okm_core::{KeyEncode, Reverse, DocumentEncode};

#[derive(KeyEncode)]
#[ok_ns(1)]
struct K {
    a: u32,
}

#[derive(DocumentEncode)]
#[ok_ref(K)]
#[ok_ns(403)]
struct R {
    f: Reverse<f64>,
}

fn main() {}
