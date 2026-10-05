//! Compile-pass and compile-fail checks for every bloomery authoring macro,
//! one test per macro family so each runs in its own process.

#[test]
fn view_ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/view/pass_handlers.rs");
    t.compile_fail("tests/ui/view/fail_signatures.rs");
    t.compile_fail("tests/ui/view/fail_impl.rs");
    t.compile_fail("tests/ui/view/fail_bare_fold.rs");
    t.compile_fail("tests/ui/view/fail_cfg.rs");
    t.compile_fail("tests/ui/view/fail_fold_attribute.rs");
    t.compile_fail("tests/ui/view/fail_conflicting_name.rs");
}

#[test]
fn reactor_ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/reactor/pass_source_publication.rs");
    t.pass("tests/ui/reactor/pass_shared_and_refutable.rs");
    t.pass("tests/ui/reactor/pass_export_ordinary_only.rs");
    t.compile_fail("tests/ui/reactor/fail_async.rs");
    t.compile_fail("tests/ui/reactor/fail_mutable.rs");
    t.compile_fail("tests/ui/reactor/fail_named_state.rs");
    t.compile_fail("tests/ui/reactor/fail_interior_mutable_state.rs");
    t.compile_fail("tests/ui/reactor/fail_tuple_state.rs");
    t.compile_fail("tests/ui/reactor/fail_context.rs");
    t.compile_fail("tests/ui/reactor/fail_no_trigger.rs");
    t.compile_fail("tests/ui/reactor/fail_option_return.rs");
    t.compile_fail("tests/ui/reactor/fail_vec_return.rs");
    t.compile_fail("tests/ui/reactor/fail_unit_return.rs");
    t.compile_fail("tests/ui/reactor/fail_invalid_names.rs");
    t.compile_fail("tests/ui/reactor/fail_unsupported_output.rs");
    t.compile_fail("tests/ui/reactor/fail_foreign_output_impl.rs");
}

#[test]
fn program_ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/program/pass_async_run.rs");
    t.pass("tests/ui/program/pass_sampled_http.rs");
    t.pass("tests/ui/program/pass_sampled_entropy.rs");
    t.compile_fail("tests/ui/program/fail_async_run_sync_env.rs");
    t.compile_fail("tests/ui/program/fail_async_run_no_return.rs");
    t.compile_fail("tests/ui/program/fail_sync_run_async_env.rs");
    t.compile_fail("tests/ui/program/fail_run_receiver.rs");
    t.compile_fail("tests/ui/program/fail_non_pure_mode.rs");
    t.compile_fail("tests/ui/program/fail_invalid_name.rs");
    t.compile_fail("tests/ui/program/fail_http_on_pure.rs");
    t.compile_fail("tests/ui/program/fail_entropy_on_pure.rs");
    t.compile_fail("tests/ui/program/fail_binding_before_env.rs");
    t.compile_fail("tests/ui/program/fail_binding_on_sync.rs");
    t.compile_fail("tests/ui/program/fail_unknown_api.rs");
    t.compile_fail("tests/ui/program/fail_api_target_mismatch.rs");
    t.compile_fail("tests/ui/program/fail_undocumented_input_field.rs");
    t.compile_fail("tests/ui/program/fail_undocumented_nested_variant.rs");
    t.compile_fail("tests/ui/program/fail_undocumented_program.rs");
}

#[test]
fn bundle_ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/bundle/pass_export_programs_only.rs");
    t.pass("tests/ui/bundle/pass_export_programs_and_reactors.rs");
    t.pass("tests/ui/bundle/pass_export_reactor_only.rs");
    t.pass("tests/ui/bundle/pass_export_mixed.rs");
    t.pass("tests/ui/bundle/pass_export_qualified.rs");
    t.pass("tests/ui/bundle/pass_export_alias.rs");
    t.pass("tests/ui/bundle/pass_export_reexport.rs");
    t.pass("tests/ui/bundle/pass_export_duplicate_short.rs");
    t.pass("tests/ui/bundle/pass_export_passthrough.rs");
    t.pass("tests/ui/bundle/pass_export_after.rs");
    t.pass("tests/ui/bundle/pass_export_shared_api_target.rs");
    t.pass("tests/ui/bundle/pass_export_program_submodule.rs");
    t.compile_fail("tests/ui/bundle/fail_export_no_roles.rs");
    t.compile_fail("tests/ui/bundle/fail_export_duplicate_program_name.rs");
    t.compile_fail("tests/ui/bundle/fail_export_reserved_namespace.rs");
    t.compile_fail("tests/ui/bundle/fail_export_missing_desc.rs");
    t.compile_fail("tests/ui/bundle/fail_export_duplicate_namespace.rs");
    t.compile_fail("tests/ui/bundle/fail_export_type_alias.rs");
}
