//! The repository conventions the model lanes carry into their prompt (#4647).
//!
//! The lanes used to *point* at this file — "read `CLAUDE.md` before editing" —
//! which only lands if the forked harness happens to read it. Headless Claude
//! auto-loads it; Codex reads `AGENTS.md`; Muse reads neither. So the lane read
//! it here instead, at prompt assembly, out of the tree the worker already has
//! checked out: the conventions the agent sees are the ones the subject tree
//! carries, and no copy in the lane's own text can fall out of date with them.

use std::fs;
use std::path::Path;

/// The conventions the lanes inline. `CLAUDE.md` rather than `AGENTS.md`: it is
/// the repository's fullest statement — it carries the testing doctrine and the
/// code-layout rules that the Codex-flavoured `AGENTS.md` summarizes past.
const CONVENTIONS_FILE: &str = "CLAUDE.md";

/// The conventions `tree` carries, or `None` when it carries none.
///
/// Fail-soft by design: a subject tree without a conventions file is a tree the
/// lane can still build against, so an absent file drops the section rather than
/// failing the dispatch.
pub(super) fn read(tree: &Path) -> Option<String> {
    fs::read_to_string(tree.join(CONVENTIONS_FILE)).ok()
}

/// Render `conventions` as the prompt section the lanes carry them in — whole
/// file, verbatim. Selecting sections out of it would be cheaper per dispatch
/// and would silently start delivering nothing the first time one is renamed;
/// the whole file cannot drift.
pub(super) fn section(conventions: &str) -> String {
    format!(
        "## Conventions\n\n\
         The subject tree's `{CONVENTIONS_FILE}`, inlined verbatim — the conventions this repository \
         is written to. Follow them as written. Where they and the lane instructions disagree about \
         how code in this repository is written, they win; where they describe a workflow this \
         dispatch is not running (opening pull requests, driving CI, the MCP harness), they do not \
         apply to you.\n\n\
         {conventions}"
    )
}
