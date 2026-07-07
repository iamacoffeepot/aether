//! Compile-time checks for `#[behavior]`: a pass case exercising a `&mut K`
//! intercept, a `&K` observe, an `#[on_attach]`, and a derived-serde state
//! struct, and a fail case where a malformed handler signature yields the
//! pointed error. A proc-macro pass/fail suite catches codegen breakage the
//! macro owns.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_behavior.rs");
    t.compile_fail("tests/ui/fail_bad_handler_signature.rs");
}
