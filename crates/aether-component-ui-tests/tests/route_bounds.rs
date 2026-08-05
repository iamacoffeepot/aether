//! Compile-time contract for component mailbox retyping (issue #4481).

#[test]
fn rejects_non_embedded_component_routes() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/rejects_non_embedded_component_routes.rs");
}
