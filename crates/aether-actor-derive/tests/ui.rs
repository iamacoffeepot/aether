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
    // ADR-0112: the manual reply class compiles. The native manual-class
    // behavior is covered by the `manual_handler_replies_through_ctx`
    // integration test in `aether-substrate` (this proc-macro crate has no
    // `aether-substrate` dev-dep, so a native *pass* / type-error fixture
    // can't link the substrate types — the existing native fixtures here
    // are all macro-level diagnostics that fire before path resolution).
    t.pass("tests/ui/accepts_manual_handler_wasm.rs");
    t.compile_fail("tests/ui/rejects_duplicate_handler_kind_wasm.rs");
    t.compile_fail("tests/ui/rejects_duplicate_handler_kind_native.rs");
    t.compile_fail("tests/ui/rejects_missing_namespace_wasm.rs");
    t.compile_fail("tests/ui/rejects_missing_namespace_native.rs");
    t.compile_fail("tests/ui/rejects_stray_const_wasm.rs");
    t.compile_fail("tests/ui/rejects_stray_const_native.rs");
    // ADR-0112: `#[handler::stream]` is reserved (a macro error, so the
    // native fixture works too); a wasm marker / class disagreement fails
    // to unify.
    t.compile_fail("tests/ui/rejects_stream_reserved_wasm.rs");
    t.compile_fail("tests/ui/rejects_stream_reserved_native.rs");
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
    // ADR-0123 struct-hosted `#[actor]` diagnostics. An unrecognised arg fails
    // at parse; the disk-read harvest hard-errors on a missing runtime module,
    // a runtime module with no `#[handler]`-bearing impl, and a handler-bearing
    // impl that omits `const NAMESPACE`. (The `local_file() == None` path under
    // `--remap-path-prefix` is not trybuild-reproducible — it is covered by the
    // hard-error branch in `harvest_runtime_identity` and exercised live.) The
    // `rt_nohandler.rs` / `rt_nonamespace.rs` siblings are read off disk by the
    // harvest, never compiled as fixtures.
    t.compile_fail("tests/ui/rejects_actor_unknown_arg.rs");
    t.compile_fail("tests/ui/rejects_struct_missing_runtime.rs");
    t.compile_fail("tests/ui/rejects_struct_no_handler.rs");
    t.compile_fail("tests/ui/rejects_struct_no_namespace.rs");
    // Issue #2460: sharpen the handler-shape diagnostics. A `&[K]` slice
    // handler is native-only (the wasm dispatcher decodes a single `K`),
    // a non-`Single` class on a `#[handler(task)]` is discarded so it is
    // rejected, and a wasm `#[handler]`'s non-`self` first param earns the
    // generalized `&self` or `&mut self` diagnostic.
    t.compile_fail("tests/ui/rejects_slice_handler_wasm.rs");
    t.compile_fail("tests/ui/rejects_manual_task_handler_native.rs");
    t.compile_fail("tests/ui/rejects_nonself_handler_wasm.rs");
}
