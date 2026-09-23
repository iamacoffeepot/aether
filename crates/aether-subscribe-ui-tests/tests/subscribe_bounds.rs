//! Compile-time contract for the bounds on the flat `ctx.subscribe::<P, K>()`
//! verb (issues #5723, #6454): the publisher publishes the kind, the
//! subscriber declared the publisher, and the subscriber's handler is silent.

#[test]
fn rejects_subscribe_to_unpublished_kind() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/rejects_subscribe_to_unpublished_kind.rs");
}

#[test]
fn rejects_subscribe_with_replying_handler() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/rejects_subscribe_with_replying_handler.rs");
}

#[test]
fn rejects_subscribe_to_undeclared_publisher() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/rejects_subscribe_to_undeclared_publisher.rs");
}
