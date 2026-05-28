//! `#[derive(Config)]` trybuild fixtures (ADR-0090 unit g,
//! iamacoffeepot/aether#1264).
//!
//! Pattern mirrors `aether-data-derive/tests/transform.rs`: a single
//! entry point that hands the `tests/ui/` directory to `trybuild`. The
//! per-fixture `.rs` files exercise the macro's accept/reject surface;
//! each compile-fail fixture pairs with a `.stderr` checked-in alongside.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/accepts_minimal.rs");
    t.pass("tests/ui/accepts_full_http.rs");
    t.pass("tests/ui/accepts_optional_field.rs");
    t.pass("tests/ui/accepts_ms_duration.rs");
    t.compile_fail("tests/ui/rejects_ms_duration_on_non_duration.rs");
    t.compile_fail("tests/ui/rejects_missing_env_prefix.rs");
    t.compile_fail("tests/ui/rejects_unknown_hint.rs");
}
