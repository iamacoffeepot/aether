//! Which changed paths are gate *inputs* and which are changes to the *tool*
//! that runs the gates (#6001).
//!
//! Both live in xtask, and reading them the same way is what turned a candidate
//! confined to `xtask/src/affected/graph.rs` into a sixty-crate run: nothing in
//! the workspace depends on xtask, so linkage says the change reaches one
//! package, and the widening came from xtask being the thing that executes
//! every gate — a change to it could in principle move every verdict.
//!
//! That argument is only true of some of xtask. The code under
//! `xtask/src/transform/verify/` *is* the gate: it decides which crates each
//! member compiles, how a member's exit code is read, and what excuses a
//! failure, so a change there can move a verdict on a crate it never mentions
//! and the whole workspace is the honest scope. The rest of xtask — the
//! selection graph, the dist builder, the operator commands — is a tool the
//! gate invokes. A change to it is proved by compiling xtask's own closure and
//! then running each gate once over [`SMOKE_PACKAGE`]: if the tool still lints,
//! documents, and tests one real crate end to end, it still works as a tool.
//!
//! The repo-wide gate configurations sit on the first side for the same reason
//! the gate code does: `clippy.toml` renames a lint rule for every crate,
//! `rustfmt.toml` reshapes what `cargo fmt -- --check` says about every file,
//! and the workspace manifest reshapes the build graph itself. None of them is
//! provable by one crate.

/// Paths whose change moves what a gate says about crates the diff never
/// entered, so the run keeps every workspace crate.
///
/// `Cargo.lock`, `rust-toolchain.toml`, `.cargo/`, `.config/` and `ci.yml` are
/// the same class and are screened one layer down, by
/// [`crate::affected::rules::global_screen`], which the selection lane shares.
/// What lives here is the half of the rule that lane does not have: the
/// distinction between the gate and the tool, which only matters to a run that
/// is deciding how much of the tree to compile.
const WHOLE_WORKSPACE_EXACT: &[&str] = &["Cargo.toml", "clippy.toml", "rustfmt.toml"];

/// The gate code itself — everything under the module that computes a member's
/// scope, dispatches it, and reads its verdict.
const GATE_PREFIX: &str = "xtask/src/transform/verify/";

/// The tool the gate runs through. Every path under it that is not gate code is
/// proved by xtask's own closure plus the smoke crate.
const TOOL_PREFIX: &str = "xtask/";

/// The crate each gate runs once over when only the tool changed.
///
/// Small, `no_std`, no dev-dependency on a harness, and compiled to no
/// component wasm — so the smoke check is one quick pass through
/// clippy / docs / test rather than a second workspace build wearing a
/// different name. What it proves is that the tool still drives a real crate
/// through every gate; which crate that is does not matter beyond being real
/// and cheap.
pub(super) const SMOKE_PACKAGE: &str = "aether-math";

/// How a candidate diff reads against the gate-versus-tool rule.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ToolChange<'a> {
    /// The diff changed the gate code or a repo-wide gate configuration: it can
    /// move a verdict anywhere, so the run keeps the whole workspace. Carries
    /// the path that said so.
    Gate(&'a str),
    /// The diff changed xtask outside the gate code, and nothing else that
    /// widens: the run compiles xtask's own closure and smoke-checks the tool
    /// over [`SMOKE_PACKAGE`]. Carries the path that said so.
    Tool(&'a str),
    /// The diff says nothing about either — an ordinary candidate, resolved by
    /// the package graph like any other.
    Ordinary,
}

impl<'a> ToolChange<'a> {
    /// Read `changed` against the rule.
    ///
    /// [`Self::Gate`] wins over [`Self::Tool`] whenever both are in one diff:
    /// the widening reason is that *some* path in the candidate can move a
    /// verdict anywhere, and a narrower sibling path in the same diff does not
    /// take that away.
    pub(super) fn of(changed: &'a [String]) -> Self {
        let paths = || changed.iter().map(String::as_str);
        if let Some(gate) = paths().find(|path| is_gate_input(path)) {
            return Self::Gate(gate);
        }
        paths().find(|path| is_tool(path)).map_or(Self::Ordinary, Self::Tool)
    }
}

/// Whether `path` is the gate code or a repo-wide gate configuration.
fn is_gate_input(path: &str) -> bool {
    WHOLE_WORKSPACE_EXACT.contains(&path) || path.starts_with(GATE_PREFIX)
}

/// Whether `path` is a change to the tool the gates run through.
///
/// Callers reach this after [`is_gate_input`] has already answered no, but it
/// is stated rather than assumed: a path under the gate prefix is never a tool
/// path, whichever order the two are asked in.
pub(super) fn is_tool(path: &str) -> bool {
    path.starts_with(TOOL_PREFIX) && !path.starts_with(GATE_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::{SMOKE_PACKAGE, ToolChange, is_tool};
    use crate::affected::graph::Workspace;

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn the_gate_code_and_the_repo_wide_gate_configs_keep_the_whole_workspace() {
        // Tripwire for the half of #6001 that must not narrow. Each of these
        // moves what a gate says about crates the diff never entered — the
        // member scope computation, the lint rule set, the format rule set, the
        // workspace build graph — so a closure computed from it is not a
        // statement about what the change can reach.
        for path in [
            "xtask/src/transform/verify/mod.rs",
            "xtask/src/transform/verify/scope.rs",
            "xtask/src/transform/verify/inputs.rs",
            "clippy.toml",
            "rustfmt.toml",
            "Cargo.toml",
        ] {
            assert_eq!(ToolChange::of(&strings(&[path])), ToolChange::Gate(path), "{path} is a gate input");
        }
    }

    #[test]
    fn xtask_outside_the_gate_code_is_a_tool_change() {
        // Tripwire for the half of #6001 that must narrow. `affected/graph.rs`
        // is the measured case: it took #5951's verify and every shared run it
        // joined to sixty crates, on the strength of xtask being the thing that
        // runs the gates rather than of anything linking it.
        for path in [
            "xtask/src/affected/graph.rs",
            "xtask/src/dist/mod.rs",
            "xtask/src/main.rs",
            "xtask/src/bloom/roll/coverage.rs",
            "xtask/Cargo.toml",
        ] {
            assert_eq!(ToolChange::of(&strings(&[path])), ToolChange::Tool(path), "{path} is a tool change");
            assert!(is_tool(path));
        }
    }

    #[test]
    fn an_ordinary_diff_says_nothing_about_either() {
        for path in ["crates/aether-math/src/lib.rs", "docs/guide/testing.md", "Cargo.lock"] {
            assert_eq!(ToolChange::of(&strings(&[path])), ToolChange::Ordinary, "{path} is neither");
            assert!(!is_tool(path));
        }
    }

    #[test]
    fn a_diff_carrying_both_keeps_the_wider_answer() {
        // Tripwire for the precedence. A candidate that edits the gate and a
        // tool module in one change still moves every verdict, and reading the
        // tool path first would narrow the run on the strength of the half that
        // proves the least.
        assert_eq!(
            ToolChange::of(&strings(&["xtask/src/affected/graph.rs", "xtask/src/transform/verify/mod.rs"])),
            ToolChange::Gate("xtask/src/transform/verify/mod.rs"),
        );
    }

    #[test]
    fn the_smoke_crate_is_a_real_workspace_member() {
        // Tripwire: the smoke check is a `-p SMOKE_PACKAGE` on every gate, so a
        // renamed or removed crate turns the narrowed xtask run into a cargo
        // error on a candidate that did nothing wrong.
        let workspace = Workspace::load().expect("load the workspace graph");

        assert!(workspace.members().contains(SMOKE_PACKAGE), "{SMOKE_PACKAGE} must be a workspace member");
        assert!(
            !workspace.wasm_sources().contains(SMOKE_PACKAGE),
            "a component crate would widen the run it is meant to keep narrow",
        );
    }
}
