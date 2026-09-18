//! Compile-pass and compile-fail checks for `#[program]` / `bundle_programs`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_bundle.rs");
    t.compile_fail("tests/ui/fail_async_run.rs");
    t.compile_fail("tests/ui/fail_run_receiver.rs");
    t.compile_fail("tests/ui/fail_non_pure_mode.rs");
    t.compile_fail("tests/ui/fail_duplicate_name.rs");
    t.compile_fail("tests/ui/fail_no_programs.rs");
    t.compile_fail("tests/ui/fail_reserved_namespace.rs");
}
