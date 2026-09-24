//! Compile-pass and compile-fail checks for `#[program]`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_async_run.rs");
    t.pass("tests/ui/pass_sampled_http.rs");
    t.compile_fail("tests/ui/fail_async_run_sync_env.rs");
    t.compile_fail("tests/ui/fail_async_run_no_return.rs");
    t.compile_fail("tests/ui/fail_sync_run_async_env.rs");
    t.compile_fail("tests/ui/fail_run_receiver.rs");
    t.compile_fail("tests/ui/fail_non_pure_mode.rs");
    t.compile_fail("tests/ui/fail_invalid_name.rs");
    t.compile_fail("tests/ui/fail_http_on_pure.rs");
    t.compile_fail("tests/ui/fail_binding_before_env.rs");
    t.compile_fail("tests/ui/fail_binding_on_sync.rs");
    t.compile_fail("tests/ui/fail_unknown_api.rs");
    t.compile_fail("tests/ui/fail_api_target_mismatch.rs");
}
