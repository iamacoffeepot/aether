//! The repository conventions the model lanes carry into their prompt (#4647).
//!
//! The lanes used to *point* at `CLAUDE.md` — "read it before editing" — which
//! only lands if the forked harness happens to read it. Headless Claude
//! auto-loads it; Muse reads neither. So the authorized bundle inlines the
//! curated lane context as its `conventions` field, and the transform renders
//! that field rather than reading a file from the checkout.

/// Render the curated lane context as the prompt section the import command
/// records on the bundle. The transform no longer reads this file; it consumes
/// the authorized bundle's `conventions` field.
pub fn section(lane_context: &str) -> String {
    format!(
        "## Conventions\n\n\
         The curated lane context — the conventions this repository is written to. Follow them as \
         written. Where they and the lane instructions disagree about how code in this repository \
         is written, they win; where they describe a workflow this dispatch is not running \
         (opening pull requests, driving CI), they do not apply to you.\n\n\
         {lane_context}"
    )
}
