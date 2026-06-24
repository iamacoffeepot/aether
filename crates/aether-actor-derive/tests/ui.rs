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
    // `runtime_feature = "name"` gate override and `one_per = "entity"`
    // instance-cardinality, to `#[bridge]` parity. The substrate-typed runtime
    // impls cfg out in the fixture bin (no `runtime`/named feature), so the pass
    // fixtures exercise the marker + name-inventory surface the args drive;
    // `one_per` without `instanced` is a macro-level rejection.
    t.pass("tests/ui/accepts_actor_runtime_feature.rs");
    t.pass("tests/ui/accepts_actor_one_per.rs");
    t.compile_fail("tests/ui/rejects_actor_one_per_without_instanced.rs");
}
