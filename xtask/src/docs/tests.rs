//! The CI gate for `cargo xtask docs check-mcp-tools`.
//!
//! `cargo nextest run -p xtask` runs on every pull request, so running the
//! real comparison here is what makes the check a merge gate — no new
//! `verify.*` member and no workflow step.

use super::{check_mcp_tools, repo_root, tool_name_drift};

/// The extractors are the whole check: if either stops matching, the diff
/// compares an empty set against an empty set and passes over anything.
const ROUTER_FIXTURE: &str = r#"
#[tool_router]
impl Mcp {
    #[tool(
        description = "..."
    )]
    pub async fn list_engines(&self, Parameters(args): Parameters<ListEnginesArgs>) -> Result<String, McpError> {
        engine::list_engines(self, args).await
    }

    // #[tool( in a comment is prose, not a registration.
    #[tool(description = "...")]
    pub async fn capture_frame(
        &self,
        Parameters(args): Parameters<CaptureFrameArgs>,
    ) -> Result<CallToolResult, McpError> {
        capture::capture_frame(self, args).await
    }

    /// A helper the router calls but does not register.
    pub async fn not_a_tool(&self) -> Result<String, McpError> {
        Ok(String::new())
    }
}
"#;

const CLAUDE_MD_FIXTURE: &str = "\
Tools (`mcp__aether-hub__*`):

- `list_engines(show?)` — an object `{engines, recently_died}` whose `engine_id` you pass on.
- `capture_frame(engine_id, window_id, mails?)` — synchronous PNG readback.

## Test harnesses
";

#[test]
fn the_extractors_read_both_sides_of_the_real_shapes() {
    assert_eq!(super::registered_tools(ROUTER_FIXTURE), ["capture_frame", "list_engines"]);
    assert_eq!(super::documented_tools(CLAUDE_MD_FIXTURE), ["capture_frame", "list_engines"]);
    assert_eq!(tool_name_drift(ROUTER_FIXTURE, CLAUDE_MD_FIXTURE), None);
}

#[test]
fn drift_in_either_direction_is_reported() {
    let missing_bullet =
        CLAUDE_MD_FIXTURE.replace("- `capture_frame(engine_id, window_id, mails?)` — synchronous PNG readback.\n", "");
    let report = tool_name_drift(ROUTER_FIXTURE, &missing_bullet).expect("an undocumented tool is drift");
    assert!(report.contains("registered but not documented"), "{report}");
    assert!(report.contains("capture_frame"), "{report}");

    let retired = CLAUDE_MD_FIXTURE
        .replace("\n## Test", "- `send_mail_untraced(mails)` — a tool that no longer exists.\n\n## Test");
    let report = tool_name_drift(ROUTER_FIXTURE, &retired).expect("a stale bullet is drift");
    assert!(report.contains("documented but not registered"), "{report}");
    assert!(report.contains("send_mail_untraced"), "{report}");
}

#[test]
fn claude_md_documents_exactly_the_registered_tools() {
    check_mcp_tools(&repo_root().expect("workspace root")).expect("CLAUDE.md's MCP tool list matches the router");
}
