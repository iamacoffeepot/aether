//! Catches a generator that accepts a malformed bundle or rejects a valid one.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_export_programs_only.rs");
    t.pass("tests/ui/pass_export_programs_and_reactors.rs");
    t.pass("tests/ui/pass_export_reactor_only.rs");
    t.pass("tests/ui/pass_export_mixed.rs");
    t.pass("tests/ui/pass_export_qualified.rs");
    t.pass("tests/ui/pass_export_alias.rs");
    t.pass("tests/ui/pass_export_reexport.rs");
    t.pass("tests/ui/pass_export_duplicate_short.rs");
    t.pass("tests/ui/pass_export_passthrough.rs");
    t.pass("tests/ui/pass_export_after.rs");
    t.pass("tests/ui/pass_export_shared_api_target.rs");
    t.pass("tests/ui/pass_export_program_submodule.rs");
    t.compile_fail("tests/ui/fail_export_no_roles.rs");
    t.compile_fail("tests/ui/fail_export_duplicate_program_name.rs");
    t.compile_fail("tests/ui/fail_export_reserved_namespace.rs");
    t.compile_fail("tests/ui/fail_export_missing_desc.rs");
    t.compile_fail("tests/ui/fail_export_default_reactor.rs");
    t.compile_fail("tests/ui/fail_export_duplicate_namespace.rs");
    t.compile_fail("tests/ui/fail_export_type_alias.rs");
    t.compile_fail("tests/ui/fail_export_default_listed_public.rs");
}
