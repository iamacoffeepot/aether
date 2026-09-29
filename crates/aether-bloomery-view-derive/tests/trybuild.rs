//! Compile-pass and compile-fail coverage for #[view] / #[fold].

#[test]
fn ui() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/pass_handlers.rs");
    cases.pass("tests/ui/pass_async_and_portable.rs");
    cases.compile_fail("tests/ui/fail_signatures.rs");
    cases.compile_fail("tests/ui/fail_async_signatures.rs");
    cases.compile_fail("tests/ui/fail_async_not_send.rs");
    cases.compile_fail("tests/ui/fail_impl.rs");
    cases.compile_fail("tests/ui/fail_bare_fold.rs");
    cases.compile_fail("tests/ui/fail_cfg.rs");
    cases.compile_fail("tests/ui/fail_fold_attribute.rs");
    cases.compile_fail("tests/ui/fail_conflicting_name.rs");
}
