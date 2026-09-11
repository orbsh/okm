use okm_core::{KeyEncode, Reverse};

#[derive(KeyEncode)]
#[kv_ns(1)]
struct Bad {
    ts: Reverse<u64>,
}

fn main() {}
