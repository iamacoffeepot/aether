//! Compile-fail `#[actor]` / `#[handler]` diagnostics (shard A of #5133).
//!
//! Pass-free so trybuild takes the `cargo check --bins --keep-going` path.
//! Suite layout and `.stderr` regeneration: `tests/ui/shard_support.rs`.

#[path = "ui/shard_support.rs"]
mod shard_support;

#[test]
fn ui_a() {
    let t = shard_support::cases("ui_a");
    t.compile_fail("tests/ui/rejects_actor_child_of_cardinality_native.rs");
    t.compile_fail("tests/ui/rejects_actor_child_of_cardinality_wasm.rs");
    t.compile_fail("tests/ui/rejects_actor_root_wasm.rs");
    t.compile_fail("tests/ui/rejects_duplicate_actor_lineage.rs");
    t.compile_fail("tests/ui/rejects_malformed_actor_lineage.rs");
    t.compile_fail("tests/ui/rejects_actor_composable_cardinality.rs");
    t.compile_fail("tests/ui/rejects_actor_composable_child_of.rs");
    t.compile_fail("tests/ui/rejects_actor_composable_native.rs");
    t.compile_fail("tests/ui/rejects_malformed_actor_composable.rs");
    t.compile_fail("tests/ui/rejects_wasm_child_spawn_without_placement.rs");
    t.compile_fail("tests/ui/rejects_generic_native_lineage_impl.rs");
    t.compile_fail("tests/ui/rejects_generic_native_lineage_struct.rs");
    t.compile_fail("tests/ui/rejects_handler_set_without_body.rs");
    t.compile_fail("tests/ui/rejects_handler_set_duplicate_adoption.rs");
    t.compile_fail("tests/ui/rejects_duplicate_handler_kind_wasm.rs");
    t.compile_fail("tests/ui/rejects_duplicate_handler_kind_native.rs");
    t.compile_fail("tests/ui/rejects_missing_namespace_wasm.rs");
    t.compile_fail("tests/ui/rejects_missing_namespace_native.rs");
    t.compile_fail("tests/ui/rejects_stray_const_wasm.rs");
    t.compile_fail("tests/ui/rejects_stray_const_native.rs");
    // ADR-0134: a `#[handler::multi]` without the required `Multi<K>` ctx
    // marker earns a pointed macro error; a non-`()` return is rejected on
    // the native path; and `#[handler::multi(task)]` is rejected like any
    // class-marked task handler.
    t.compile_fail("tests/ui/rejects_multi_marker_mismatch_wasm.rs");
    t.compile_fail("tests/ui/rejects_multi_nonunit_return_native.rs");
    t.compile_fail("tests/ui/rejects_multi_task_handler_native.rs");
    t.compile_fail("tests/ui/rejects_manual_marker_mismatch_wasm.rs");
    // ADR-0112 (single-locked): a single-class `#[handler]` body has no
    // reply surface (`OutboundReply` is not impl'd for the `Single` ctx),
    // so a hand-call to `ctx.reply` is a compile error — `-> ()` is
    // provably silent.
    t.compile_fail("tests/ui/single_handler_cannot_reply.rs");
    // ADR-0113: the macro enforces the XOR (no manual hook), the pairing
    // (both accessors), and the dependency (an accessor needs `type State`).
    t.compile_fail("tests/ui/rejects_state_with_manual_hook.rs");
    t.compile_fail("tests/ui/rejects_accessor_without_state.rs");
    t.compile_fail("tests/ui/rejects_missing_rehydrate.rs");
    // ADR-0123 struct-hosted `#[actor]` diagnostics. An unrecognised arg fails
    // at parse; the disk-read harvest hard-errors on a missing runtime module,
    // a runtime module with no `#[handler]`-bearing `impl NativeActor`, a
    // handler-bearing impl that omits `const NAMESPACE`, and (gap 1) a runtime
    // module with more than one `#[handler]`-bearing `impl NativeActor` — the
    // cfg-blind harvest refuses rather than silently picking the first. (The
    // `local_file() == None` path under `--remap-path-prefix` is not
    // trybuild-reproducible — it is covered by the hard-error branch in
    // `harvest_runtime_identity` and exercised live.) The `rt_nohandler.rs` /
    // `rt_nonamespace.rs` / `rt_ambiguous.rs` siblings are read off disk by the
    // harvest, never compiled as fixtures.
    t.compile_fail("tests/ui/rejects_actor_unknown_arg.rs");
    t.compile_fail("tests/ui/rejects_struct_missing_runtime.rs");
    t.compile_fail("tests/ui/rejects_struct_no_handler.rs");
    t.compile_fail("tests/ui/rejects_struct_no_namespace.rs");
    t.compile_fail("tests/ui/rejects_struct_ambiguous_runtime.rs");
    t.compile_fail("tests/ui/rejects_struct_handler_set.rs");
    // Issue #2460: sharpen the handler-shape diagnostics. A `&[K]` slice
    // handler is native-only (the wasm dispatcher decodes a single `K`),
    // a non-`Single` class on a `#[handler(task)]` is discarded so it is
    // rejected, and a wasm `#[handler]`'s non-`self` first param earns the
    // generalized `&self` or `&mut self` diagnostic.
    t.compile_fail("tests/ui/rejects_slice_handler_wasm.rs");
    t.compile_fail("tests/ui/rejects_manual_task_handler_native.rs");
    t.compile_fail("tests/ui/rejects_duplicate_native_init.rs");
    t.compile_fail("tests/ui/rejects_nonself_handler_wasm.rs");
    // Issue #2607 (ADR-0134): bare `#[handler]` and the classless
    // `#[handler(mail)]` paren form are pointed compile errors naming all
    // three accepted class spellings; `#[handler(task)]` stays classless
    // and unaffected (proven by `accepts_actor_split_task_handler.rs`
    // continuing to compile with its bare task handler).
    t.compile_fail("tests/ui/rejects_bare_handler_wasm.rs");
    t.compile_fail("tests/ui/rejects_bare_handler_native.rs");
    t.compile_fail("tests/ui/rejects_bare_mail_variant_native.rs");
}
