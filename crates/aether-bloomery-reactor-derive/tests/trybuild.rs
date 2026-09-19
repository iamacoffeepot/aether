//! Compile-pass and compile-fail checks for `#[reactor]` / `#[rule]`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_source_publication.rs");
    t.pass("tests/ui/pass_shared_and_refutable.rs");
    t.pass("tests/ui/pass_export_ordinary_only.rs");
    t.compile_fail("tests/ui/fail_async.rs");
    t.compile_fail("tests/ui/fail_mutable.rs");
    t.compile_fail("tests/ui/fail_named_state.rs");
    t.compile_fail("tests/ui/fail_interior_mutable_state.rs");
    t.compile_fail("tests/ui/fail_tuple_state.rs");
    t.compile_fail("tests/ui/fail_context.rs");
    t.compile_fail("tests/ui/fail_no_trigger.rs");
    t.compile_fail("tests/ui/fail_option_return.rs");
    t.compile_fail("tests/ui/fail_vec_return.rs");
    t.compile_fail("tests/ui/fail_unit_return.rs");
    t.compile_fail("tests/ui/fail_invalid_names.rs");
}
