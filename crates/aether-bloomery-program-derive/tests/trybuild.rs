//! Compile-pass and compile-fail checks for `#[program]`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/fail_async_run.rs");
    t.compile_fail("tests/ui/fail_run_receiver.rs");
    t.compile_fail("tests/ui/fail_non_pure_mode.rs");
}
