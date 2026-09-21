//! Compile-pass and compile-fail checks for `#[program]`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_async_run.rs");
    t.compile_fail("tests/ui/fail_async_run_sync_env.rs");
    t.compile_fail("tests/ui/fail_sync_run_async_env.rs");
    t.compile_fail("tests/ui/fail_run_receiver.rs");
    t.compile_fail("tests/ui/fail_non_pure_mode.rs");
    t.compile_fail("tests/ui/fail_invalid_name.rs");
}
