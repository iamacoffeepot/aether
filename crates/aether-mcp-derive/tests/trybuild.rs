//! The parser's refusals.
//!
//! Every fixture names one branch of the impl-block walk, and every one of them
//! is a mistake an author makes rather than a shape the emitter could not
//! reach: a tool with no way to answer, two tools claiming one name, two
//! handlers claiming one reply kind, an attribute stack in the wrong order.
//!
//! The fixtures are self-contained — each declares its own actor-shaped trait
//! and stub support types rather than depending on `aether-mcp`. That is honest
//! rather than a shortcut: every check reached here is syntactic, and the macro
//! refuses before any emitted path would be resolved, so a real dependency cone
//! would add link time per case without changing a single diagnostic.
//!
//! What the macro *accepts* is not proven here. That is the compiled fixture
//! actor in `aether-mcp`'s runtime tests, which boots the real capability and
//! drives the generated dispatchers and composite reply handler.
//!
//! Regenerate the expectations with
//! `TRYBUILD=overwrite cargo test -p aether-mcp-derive --test trybuild`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();

    // Structural refusals, raised before any method is parsed.
    t.compile_fail("tests/ui/rejects_generic_impl.rs");
    t.compile_fail("tests/ui/rejects_nonliteral_namespace.rs");
    t.compile_fail("tests/ui/rejects_expansion_after_http_router.rs");

    // The tool attribute's own grammar.
    t.compile_fail("tests/ui/rejects_duplicate_tool_name.rs");
    t.compile_fail("tests/ui/rejects_contradictory_hints.rs");
    t.compile_fail("tests/ui/rejects_unsupported_tool_return.rs");

    // Tool-to-mapping pairing: a deferred call answers exactly once, and a
    // synchronous one answers in its own dispatcher.
    t.compile_fail("tests/ui/rejects_missing_terminal_mapping.rs");
    t.compile_fail("tests/ui/rejects_repeated_terminal_mapping.rs");
    t.compile_fail("tests/ui/rejects_two_terminal_mappings.rs");
    t.compile_fail("tests/ui/rejects_mapping_on_a_synchronous_tool.rs");
    t.compile_fail("tests/ui/rejects_reply_naming_no_tool.rs");
    t.compile_fail("tests/ui/rejects_two_fallback_owners.rs");

    // The marker's own fallback expansion, reached only without a router.
    t.compile_fail("tests/ui/rejects_tool_without_router.rs");
}
