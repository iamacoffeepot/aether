//! The repository conventions the model lanes carry into their prompt (#4647).
//!
//! The lanes used to *point* at `CLAUDE.md` — "read it before editing" — which
//! only lands if the forked harness happens to read it. Headless Claude
//! auto-loads it; Codex reads `AGENTS.md`; Muse reads neither. So the lane
//! inlines the conventions here, at prompt assembly. #5141 curates that
//! inline from [`LANE_CONTEXT`] rather than the whole subject-tree
//! `CLAUDE.md`: MCP / runtime / wasm / pipeline workflow have no lane tool
//! surface, and a missing file is a compile error rather than a silent omit.

/// The curated conventions every model lane inlines (#5141). Sibling of the
/// instruction sources; `include_str!` so a missing file fails the xtask
/// build — assembly cannot drop the section the way a missing `CLAUDE.md`
/// used to.
pub(super) const LANE_CONTEXT: &str = include_str!("lane_context.md");

/// Render the curated lane context as the prompt section the lanes carry it in.
pub(super) fn section() -> String {
    format!(
        "## Conventions\n\n\
         The curated lane context — the conventions this repository is written to. Follow them as \
         written. Where they and the lane instructions disagree about how code in this repository \
         is written, they win; where they describe a workflow this dispatch is not running \
         (opening pull requests, driving CI), they do not apply to you.\n\n\
         {LANE_CONTEXT}"
    )
}
