//! The verify lane's tool dependency graph and its preflight (#4706).
//!
//! A verify member is an external program, and some of those programs need
//! other programs. A host missing one cannot compute the answer the member
//! exists to give — so it must say that, loudly, *before* any work is
//! dispatched against it.
//!
//! The alternative is worse than it looks. A missing tool that reports "skipped"
//! is a check that silently stops being a check: the candidate passes verify,
//! integrates, folds, passes the aggregate, and the first thing to disagree is
//! the landing pull request's CI — after a model has been paid for every stage
//! in between. A missing tool is a broken host, not a softer gate.

use std::collections::BTreeSet;
use std::process::{Command, Stdio};

/// One external program a verify member runs, and what it in turn needs.
///
/// The `requires` edge is what makes this a graph rather than a list: `jscpd`
/// runs through `npx`, which is useless without `node`. Reporting "jscpd is
/// missing" to an operator whose actual problem is that Node is not installed
/// sends them to the wrong place.
struct Tool {
    /// The executable resolved on `PATH`.
    program: &'static str,
    /// The programs this one needs before it can run.
    requires: &'static [&'static str],
    /// What an operator runs to get it — reported verbatim, so a failing
    /// preflight is a fix list rather than a diagnosis exercise.
    install: &'static str,
}

/// Every program the verify lane can reach, and its own prerequisites.
///
/// `cargo` and `node` are the roots: they anchor the graph and have no
/// prerequisite this repository can state (a machine without them cannot build
/// anything at all).
const TOOLS: &[Tool] = &[
    Tool { program: "cargo", requires: &[], install: "install a Rust toolchain via https://rustup.rs" },
    Tool { program: "node", requires: &[], install: "install Node.js (https://nodejs.org)" },
    Tool { program: "npx", requires: &["node"], install: "ships with Node.js" },
    Tool { program: "rustfmt", requires: &["cargo"], install: "rustup component add rustfmt" },
    Tool { program: "cargo-clippy", requires: &["cargo"], install: "rustup component add clippy" },
    Tool { program: "cargo-nextest", requires: &["cargo"], install: "cargo install cargo-nextest --locked" },
    Tool { program: "cargo-machete", requires: &["cargo"], install: "cargo install cargo-machete --locked" },
];

/// The entry for `program`, if the graph knows it.
fn tool(program: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|entry| entry.program == program)
}

/// Every program `roots` transitively needs, including the roots themselves.
///
/// Iterative over an explicit work stack rather than recursive: the graph is
/// tiny today, but a cycle in a hand-authored table would be a stack overflow
/// rather than an error, and the visited set makes one a no-op instead.
fn closure(roots: &[&str]) -> BTreeSet<&'static str> {
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    let mut pending: Vec<&'static str> = roots.iter().filter_map(|name| tool(name).map(|t| t.program)).collect();

    while let Some(program) = pending.pop() {
        if !seen.insert(program) {
            continue;
        }
        if let Some(entry) = tool(program) {
            pending.extend(entry.requires.iter().copied());
        }
    }
    seen
}

/// Whether `program` resolves to something runnable on this host.
///
/// `--version` rather than a `PATH` scan: a cargo subcommand is not always a
/// bare file on `PATH` (cargo resolves `cargo-machete` for `cargo machete`),
/// and a file that exists but cannot execute is not a tool that is present.
fn is_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A tool the host does not have, paired with how to get it.
pub(super) struct Missing {
    pub(super) program: &'static str,
    pub(super) install: &'static str,
}

/// Resolve `roots` through the graph and report **every** missing program.
///
/// Every one, not the first: the same rule the lane itself follows. An operator
/// who installs one tool, re-runs, and is told about the next has been handed
/// the drip-feed this whole change exists to end.
pub(super) fn preflight(roots: &[&str]) -> Vec<Missing> {
    closure(roots)
        .into_iter()
        .filter(|program| !is_available(program))
        .filter_map(|program| tool(program).map(|entry| Missing { program: entry.program, install: entry.install }))
        .collect()
}

/// The findings prose a failed preflight produces.
///
/// Shaped as an operator instruction rather than a repair brief, because the
/// reader is not the model: no candidate can fix a host that lacks a compiler,
/// so directing a `Refine` at this would spend an attempt to learn nothing.
pub(super) fn missing_findings(missing: &[Missing]) -> String {
    let list = missing
        .iter()
        .map(|entry| format!("- `{}` — {}", entry.program, entry.install))
        .collect::<Vec<String>>()
        .join("\n");

    format!(
        "Verification did not run. This host is missing tools the verify lane needs, so it \
         cannot compute whether the candidate passes — which is not the same as the candidate \
         failing, and no change to the candidate can fix it.\n\n{list}\n\n\
         Install these on the executor host and re-dispatch."
    )
}

#[cfg(test)]
mod tests {
    use super::{Missing, closure, missing_findings, preflight};

    #[test]
    fn a_tools_prerequisites_come_with_it() {
        // Tripwire: the edge is the whole reason this is a graph. `jscpd` runs
        // through `npx`, which is inert without `node` — a preflight that
        // checked only the directly-named program would clear a host that
        // cannot run the check, which is the false green this exists to stop.
        let resolved = closure(&["npx"]);

        assert!(resolved.contains("npx"));
        assert!(resolved.contains("node"), "npx without node is not a runnable npx");
    }

    #[test]
    fn an_unknown_program_resolves_to_nothing_rather_than_itself() {
        // A member naming a tool the graph does not carry is an authoring bug.
        // It must not silently pass preflight by being treated as its own
        // dependency — the graph is the source of truth for what can be checked.
        assert!(closure(&["not-a-real-tool"]).is_empty());
    }

    #[test]
    fn cargo_is_present_wherever_these_tests_run() {
        // Not a mirror: it exercises the real `--version` probe against a real
        // host. If `is_available` regressed to something that never succeeds,
        // every preflight would fail closed and every bloom would refuse.
        assert!(preflight(&["cargo"]).is_empty(), "cargo must resolve in any environment running this suite");
    }

    #[test]
    fn the_findings_name_every_missing_tool_and_its_install() {
        let rendered = missing_findings(&[
            Missing { program: "cargo-nextest", install: "cargo install cargo-nextest --locked" },
            Missing { program: "node", install: "install Node.js (https://nodejs.org)" },
        ]);

        assert!(rendered.contains("cargo-nextest"));
        assert!(rendered.contains("cargo install cargo-nextest --locked"));
        assert!(rendered.contains("node"), "one missing tool must not eclipse the other");
        assert!(
            rendered.contains("cannot compute"),
            "the reader must be told this is a host fault, not a candidate one"
        );
    }
}
