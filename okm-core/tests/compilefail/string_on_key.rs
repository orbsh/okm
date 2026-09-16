use okm_core::KeyEncode;

#[derive(KeyEncode)]
#[ok_ns(1)]
struct Bad {
    name: String,
}

fn main() {}
