//! `#[actor]` macro trybuild fixtures (iamacoffeepot/aether#1553).
//!
//! The `tests/ui/` fixtures exercise the spanned diagnostics the
//! `#[actor]` macro emits on BOTH direct expansion paths — wasm
//! (`impl FfiActor for X`) and native (`impl NativeActor for X`) — so a
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
    t.compile_fail("tests/ui/rejects_duplicate_handler_kind_ffi.rs");
    t.compile_fail("tests/ui/rejects_duplicate_handler_kind_native.rs");
    t.compile_fail("tests/ui/rejects_missing_namespace_ffi.rs");
    t.compile_fail("tests/ui/rejects_missing_namespace_native.rs");
    t.compile_fail("tests/ui/rejects_stray_const_ffi.rs");
    t.compile_fail("tests/ui/rejects_stray_const_native.rs");
}
