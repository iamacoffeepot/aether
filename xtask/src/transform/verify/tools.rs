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
use std::path::Path;
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

/// Something the host does not have — a program or a toolchain target —
/// paired with how to get it.
pub(super) struct Missing {
    pub(super) requirement: &'static str,
    pub(super) install: String,
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
        .filter_map(|program| {
            tool(program).map(|entry| Missing { requirement: entry.program, install: entry.install.to_owned() })
        })
        .collect()
}

/// Whether the standard library for `target` is installed for the toolchain in
/// use — the cross-compilation half of the preflight, which no `PATH` probe can
/// answer.
///
/// `rustc --print target-libdir` rather than `rustup target list --installed`:
/// the question is a property of the active toolchain, and a host whose Rust
/// came from a distribution package has no `rustup` to ask. rustc prints the
/// path whether or not the target is installed, so the directory's existence is
/// the signal — an unknown triple makes rustc itself exit non-zero.
fn target_is_installed(target: &str) -> bool {
    let probe = Command::new("rustc")
        .args(["--print", "target-libdir", "--target", target])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match probe {
        Ok(output) if output.status.success() => {
            String::from_utf8(output.stdout).is_ok_and(|libdir| Path::new(libdir.trim()).is_dir())
        }
        _ => false,
    }
}

/// Report every toolchain target in `targets` this host cannot cross-build for.
///
/// The lane's `verify.test` pre-build cross-compiles the component wasm, and CI
/// installs that target explicitly (the toolchain action's `targets:` line). A
/// host without it builds no wasm, and `AETHER_REQUIRE_RUNTIME=1` — set so a
/// missing component is loud rather than a silent skip — then fails one test per
/// scenario for a reason no candidate can fix.
pub(super) fn preflight_targets(targets: &[&'static str]) -> Vec<Missing> {
    targets
        .iter()
        .copied()
        .collect::<BTreeSet<&'static str>>()
        .into_iter()
        .filter(|target| !target_is_installed(target))
        .map(|target| Missing { requirement: target, install: format!("rustup target add {target}") })
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
        .map(|entry| format!("- `{}` — {}", entry.requirement, entry.install))
        .collect::<Vec<String>>()
        .join("\n");

    format!(
        "Verification did not run. This host is missing tools or toolchain targets the verify lane needs, so it \
         cannot compute whether the candidate passes — which is not the same as the candidate \
         failing, and no change to the candidate can fix it.\n\n{list}\n\n\
         Install these on the executor host and re-dispatch."
    )
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{Missing, closure, missing_findings, preflight, preflight_targets};

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
    fn an_uninstalled_target_is_reported_missing_with_the_command_that_installs_it() {
        // Tripwire: the target probe must fail *closed*. It runs a real rustc
        // against a triple no toolchain carries, so a regression to a probe
        // that answers "installed" without looking — the shape a `PATH` scan or
        // a swallowed spawn error takes — clears a host that cannot cross-build
        // the component wasm, and the lane then spends a full suite run
        // reporting one host fault as a failure per scenario test.
        let missing = preflight_targets(&["definitely-not-a-real-target"]);

        let [entry] = missing.as_slice() else {
            panic!("an unknown triple is missing, got {} entries", missing.len())
        };
        assert_eq!(entry.requirement, "definitely-not-a-real-target");
        assert_eq!(entry.install, "rustup target add definitely-not-a-real-target", "the report is a fix list");
    }

    #[test]
    fn the_toolchain_that_runs_these_tests_probes_as_installed() {
        // The other half of the same probe, against the one target every host
        // running this suite provably has: its own. A probe that fails closed
        // for everything — a spawn error read as absence, a libdir path never
        // resolved — would refuse every host and wedge every bloom on a
        // requirement nobody can satisfy, which is worse than the gap it
        // replaces. Leaked, so the triple stays whatever this host is.
        let host: &'static str = Box::leak(host_triple().into_boxed_str());

        assert!(preflight_targets(&[host]).is_empty(), "{host} must probe as installed");
    }

    /// This host's target triple, from `rustc -vV`'s `host:` line.
    fn host_triple() -> String {
        let output = Command::new("rustc").arg("-vV").output().expect("rustc runs wherever this suite does");
        String::from_utf8(output.stdout)
            .expect("rustc -vV is utf-8")
            .lines()
            .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
            .expect("rustc -vV reports a host triple")
    }

    #[test]
    fn the_findings_name_every_missing_tool_and_its_install() {
        let rendered = missing_findings(&[
            Missing { requirement: "cargo-nextest", install: "cargo install cargo-nextest --locked".to_owned() },
            Missing { requirement: "node", install: "install Node.js (https://nodejs.org)".to_owned() },
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
