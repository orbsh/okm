use okm_core::{KeyEncode, Reverse, ObjEncode};

#[derive(KeyEncode)]
#[ok_ns(1)]
struct K {
    a: u32,
}

#[derive(ObjEncode)]
#[ok_ref(K)]
struct R {
    f: Reverse<f64>,
}

fn main() {}
