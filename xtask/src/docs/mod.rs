//! `cargo xtask docs` — pin the hand-written documentation that mirrors a
//! declared surface.
//!
//! One check today: the MCP tool list in `CLAUDE.md`. That list is the first
//! thing an agent reads before calling a tool, and it is hand-maintained
//! against `#[tool]` registrations in another crate — so it drifts silently,
//! and a missing tool or a stale one is invisible until someone calls it.
//! This reads both sides and diffs the names.

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

/// The hand-written list under audit.
const CLAUDE_MD: &str = "CLAUDE.md";
/// The `#[tool_router]` impl that is the source of truth for it.
const TOOL_ROUTER: &str = "crates/aether-mcp/src/tools/mod.rs";
/// The line in `CLAUDE.md` that opens the tool list. The section runs to the
/// next `## ` heading.
const TOOL_SECTION_HEADER: &str = "Tools (`mcp__aether-hub__*`):";

/// `cargo xtask docs`.
#[derive(Args, Debug)]
pub struct DocsArgs {
    #[command(subcommand)]
    command: DocsCommand,
}

#[derive(Subcommand, Debug)]
enum DocsCommand {
    /// Diff the MCP tool names documented in `CLAUDE.md` against the
    /// `#[tool]`-registered set, and fail on drift.
    CheckMcpTools,
}

pub fn run(args: &DocsArgs) -> Result<()> {
    match args.command {
        DocsCommand::CheckMcpTools => check_mcp_tools(&repo_root()?),
    }
}

/// Walk up from this crate's manifest to the workspace root.
fn repo_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .context("xtask manifest dir has no parent — cannot locate the workspace root")
}

fn check_mcp_tools(root: &Path) -> Result<()> {
    let read = |relative: &str| fs::read_to_string(root.join(relative)).with_context(|| format!("reading {relative}"));

    match tool_name_drift(&read(TOOL_ROUTER)?, &read(CLAUDE_MD)?) {
        None => Ok(()),
        Some(report) => bail!("{report}"),
    }
}

/// The registered-vs-documented mismatch, or `None` when the two agree.
///
/// Kept separate from the IO so the same comparison backs both the command and
/// the test that gates it in CI.
fn tool_name_drift(router_source: &str, claude_md: &str) -> Option<String> {
    let registered = registered_tools(router_source);
    let documented = documented_tools(claude_md);

    let undocumented: Vec<&String> = registered.iter().filter(|name| !documented.contains(name)).collect();
    let unregistered: Vec<&String> = documented.iter().filter(|name| !registered.contains(name)).collect();
    if undocumented.is_empty() && unregistered.is_empty() {
        return None;
    }

    let mut report = format!(
        "the MCP tool list in {CLAUDE_MD} has drifted from the #[tool] registrations in {TOOL_ROUTER} \
         ({} registered, {} documented)",
        registered.len(),
        documented.len()
    );
    if !undocumented.is_empty() {
        let _ = write!(report, "\n  registered but not documented: {undocumented:?}");
    }
    if !unregistered.is_empty() {
        let _ = write!(report, "\n  documented but not registered: {unregistered:?}");
    }
    Some(report)
}

/// Every tool name the `#[tool_router]` impl registers: the `pub async fn` that
/// follows each `#[tool(` attribute.
fn registered_tools(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_tool_attribute = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.starts_with("#[tool(") {
            in_tool_attribute = true;
            continue;
        }
        if let Some(name) = in_tool_attribute.then(|| trimmed.strip_prefix("pub async fn ")).flatten() {
            names.push(name.split(['(', '<']).next().unwrap_or_default().trim().to_owned());
            in_tool_attribute = false;
        }
    }
    names.sort();
    names
}

/// Every tool name the `CLAUDE.md` tool section documents: the backticked
/// identifiers in each bullet's head, i.e. before the em dash that opens its
/// prose. A bullet head is `` `name` `` or `` `name(args…)` ``, and one bullet
/// may head two tools (`upload_binary` / `upload_component`).
fn documented_tools(claude_md: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_section = false;
    for line in claude_md.lines() {
        if line.trim() == TOOL_SECTION_HEADER {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if line.starts_with("## ") {
            break;
        }
        let Some(bullet) = line.strip_prefix("- ") else {
            continue;
        };
        names.extend(bullet.split_once('—').map_or(bullet, |(head, _)| head).split('`').skip(1).step_by(2).filter_map(
            |span| {
                let name = span.split('(').next().unwrap_or_default();
                is_tool_ident(name).then(|| name.to_owned())
            },
        ));
    }
    names.sort();
    names
}

fn is_tool_ident(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.starts_with(|c: char| c.is_ascii_lowercase())
        && candidate.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
