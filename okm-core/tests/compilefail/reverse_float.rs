use okm_core::{KeyEncode, Reverse, RowEncode};

#[derive(KeyEncode)]
#[kv_ns(1)]
struct K {
    a: u32,
}

#[derive(RowEncode)]
#[kv_ref(K)]
struct R {
    f: Reverse<f64>,
}

fn main() {}
