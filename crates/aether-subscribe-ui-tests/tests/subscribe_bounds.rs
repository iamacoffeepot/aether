//! Compile-time contract for the publisher gate on `subscribe` (issue #5723).

#[test]
fn rejects_subscribe_to_unpublished_kind() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/rejects_subscribe_to_unpublished_kind.rs");
}
