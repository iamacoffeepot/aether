//! `#[actor]` macro trybuild fixtures (iamacoffeepot/aether#1553).
//!
//! The `tests/ui/` fixtures exercise the spanned diagnostics the
//! `#[actor]` macro emits on BOTH direct expansion paths — wasm
//! (`impl WasmActor for X`) and native (`impl NativeActor for X`) — so a
//! malformed actor block earns a pointed error at the author's code
//! instead of a downstream type error against a generated impl:
//!
//!   - duplicate `#[handler]` mail kinds (spanned at the later handler),
//!   - a missing `const NAMESPACE` (spanned at the type),
//!   - a stray non-`NAMESPACE` const (spanned at the const).
//!
//! Each is golden-tested on both paths to keep the wasm / native
//! diagnostic surface symmetric. `.stderr` goldens are toolchain-
//! sensitive — regenerate with `TRYBUILD=overwrite cargo test -p
//! aether-actor-derive --test ui`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/accepts_minimal_actor.rs");
    t.pass("tests/ui/accepts_generic_local.rs");
    t.pass("tests/ui/accepts_actor_lineage_wasm.rs");
    t.pass("tests/ui/accepts_actor_composable_wasm.rs");
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
    // ADR-0119 parent-scope amendment: a wasm actor's macro-emitted
    // `Embedded` resolver is admitted on ordinary typed ctx and MailSender
    // surfaces, which select the caller's runtime parent mailbox.
    t.pass("tests/ui/accepts_bare_type_address_of_embedded_peer.rs");
    t.compile_fail("tests/ui/rejects_generic_native_lineage_impl.rs");
    t.compile_fail("tests/ui/rejects_generic_native_lineage_struct.rs");
    // ADR-0112: the manual reply class compiles. The native manual-class
    // behavior is covered by the `manual_handler_replies_through_ctx`
    // integration test in `aether-substrate` (this proc-macro crate has no
    // `aether-substrate` dev-dep, so a native *pass* / type-error fixture
    // can't link the substrate types — the existing native fixtures here
    // are all macro-level diagnostics that fire before path resolution).
    t.pass("tests/ui/accepts_manual_handler_wasm.rs");
    // ADR-0169: the handler-set delegation seam — the set's dispatch method
    // must be callable from the adopter's table and its manifest const usable
    // in the adopter's const-array arithmetic — plus the two shapes the macro
    // refuses outright.
    t.pass("tests/ui/accepts_handler_set_wasm.rs");
    t.compile_fail("tests/ui/rejects_handler_set_without_body.rs");
    t.compile_fail("tests/ui/rejects_handler_set_duplicate_adoption.rs");
    // ADR-0134: the multi reply class compiles on both expansion paths. The
    // wasm fixture type-checks the full `emit` body; the native fixture uses
    // the split + runtime-feature gate (like `accepts_actor_split_task_handler`)
    // so the substrate-typed runtime surface cfgs out, leaving the assertion
    // that the macro accepts the `#[handler::multi]` + `Multi<K>` signature.
    t.pass("tests/ui/accepts_multi_handler_wasm.rs");
    t.pass("tests/ui/accepts_multi_handler_native.rs");
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
    // ADR-0113: declarative `type State` + `dehydrate` / `rehydrate`
    // accessors generate the hot-swap hooks; the macro enforces the XOR
    // (no manual hook), the pairing (both accessors), and the dependency
    // (an accessor needs `type State`).
    t.pass("tests/ui/accepts_state_actor.rs");
    t.compile_fail("tests/ui/rejects_state_with_manual_hook.rs");
    t.compile_fail("tests/ui/rejects_accessor_without_state.rs");
    t.compile_fail("tests/ui/rejects_missing_rehydrate.rs");
    // iamacoffeepot/aether#2330: the `#[actor]` split path gains a
    // `runtime_feature = "name"` gate override. The substrate-typed runtime
    // impls cfg out in the fixture bin (no `runtime`/named feature), so the pass
    // fixture exercises the marker + name-inventory surface the arg drives.
    t.pass("tests/ui/accepts_actor_runtime_feature.rs");
    // iamacoffeepot/aether#2338: a split `#[actor]` may carry a `#[fallback]`
    // whose first param is `state: &mut Self::State` (the validator gained the
    // `is_split` branch the split `#[handler]` path already had).
    t.pass("tests/ui/accepts_actor_split_fallback.rs");
    // iamacoffeepot/aether#2341: a split `#[actor]` may carry a `#[handler(task)]`
    // whose first param is `state: &mut Self::State` (the last native-split
    // first-param validator to gain the `is_split` branch).
    t.pass("tests/ui/accepts_actor_split_task_handler.rs");
    // ADR-0123 struct-hosted `#[actor]` happy path: the disk-read harvest selects
    // the runtime module's `impl NativeActor` (gap-1 trait filter), lifts its
    // identity, and emits the addressing markers plus the gap-3 `include_bytes!`
    // rebuild edge. `rt_ok.rs` is the sibling stub read off disk, never compiled.
    t.pass("tests/ui/accepts_struct_hosted_actor.rs");
    // The module-path form of the same harvest: `#[actor(singleton,
    // nested::rt_nested)]` joins the path segments into a file path relative
    // to the invoking file (`nested/rt_nested.rs`) — the headless-companion
    // layout (`runtime::headless`). `nested/rt_nested.rs` is read off disk,
    // never compiled.
    t.pass("tests/ui/accepts_struct_nested_runtime.rs");
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
