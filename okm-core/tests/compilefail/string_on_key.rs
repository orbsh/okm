use okm_core::KeyEncode;

#[derive(KeyEncode)]
#[kv_ns(1)]
struct Bad {
    name: String,
}

fn main() {}
