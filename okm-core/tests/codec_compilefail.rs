//! Compile-time rejections of the codec macros: variable-length and
//! wrapper types are payload-only — the fixed-width key encoding must
//! refuse them, and Reverse must refuse non-whitelist inner types.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

#[test]
fn compile_fails() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compilefail/*.rs");
}
