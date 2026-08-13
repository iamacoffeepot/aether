//! `cargo xtask affected` — affected-package test selection for PR CI
//! (issue #3611).
//!
//! Diffs the checkout against a base ref, maps the changed paths onto the
//! workspace package graph (guppy + determinator), takes the
//! reverse-dependency closure, and reports which packages need their tests
//! run. PR CI consumes the result to run `cargo nextest run -p <set>`
//! instead of the whole workspace; pushes to `main` keep the full suite as
//! the soundness backstop, so a selection miss delays a red signal to the
//! landing commit but never loses it.
//!
//! Selection is conservative by construction: a path that shapes the build
//! graph or the test configuration forces `run_all` before any graph
//! analysis runs, and a changed path matching no workspace package and no
//! rule marks the whole workspace changed (determinator's built-in
//! fallback). The couplings cargo's graph cannot see are injected
//! structurally: a changed component or behavior crate pulls in
//! the wasm-executing scenario suites, resolved through cargo's own
//! dependency edges (the chassis-bundle coupling special case retired
//! with that crate, #3816).
//!
//! The one narrowing on top of package granularity is [`test_targets`]: a
//! path under a package's own `tests/` directory compiles into an
//! integration-test binary nothing links against, so it selects that
//! package and stops there rather than dragging in a reverse-dependency
//! closure it cannot reach (#4197). The structural properties that
//! narrowing rests on are recomputed on every push by the test-only
//! `invariants` module rather than recorded in a comment (#4215).

pub mod graph;
#[cfg(test)]
mod invariants;
pub mod rules;
mod run;
pub mod select;
mod test_targets;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::process::Command;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::affected::graph::Workspace;
use crate::affected::rules::global_screen;
use crate::affected::select::Selection;

#[derive(Args)]
pub struct AffectedArgs {
    /// Base ref to diff against (CI passes `HEAD^1` — on a PR merge
    /// commit that is the base-branch tip). Defaults to the merge-base
    /// of `origin/main` and `HEAD` for local use.
    #[arg(long)]
    base: Option<String>,

    /// Also append `run_all` / `packages` / `package_args` /
    /// `wasm_needed` lines to the file `$GITHUB_OUTPUT` points at.
    #[arg(long)]
    github_output: bool,

    /// Run the CI-equivalent test command for the computed selection.
    #[arg(long)]
    run: bool,
}

pub fn run(args: &AffectedArgs) -> Result<()> {
    let result = compute(args)?;

    report(&result.selection, result.changed_count);
    if args.github_output {
        write_github_output(&result.selection)?;
    }
    if args.run {
        run::execute(&result.selection).with_context(|| format!("run affected selection from base {}", result.base))?;
    }
    Ok(())
}

struct AffectedResult {
    base: String,
    changed_count: usize,
    selection: Selection,
}

fn compute(args: &AffectedArgs) -> Result<AffectedResult> {
    let repo_root = git_stdout(&["rev-parse", "--show-toplevel"], ".")?;
    let repo_root = repo_root.trim();
    let base = match &args.base {
        Some(base) => base.clone(),
        None => git_stdout(&["merge-base", "origin/main", "HEAD"], repo_root)?.trim().to_string(),
    };

    let diff = git_stdout(&["diff", "--name-only", "-z", &base, "HEAD"], repo_root)?;
    let changed: Vec<String> = diff.split('\0').filter(|path| !path.is_empty()).map(str::to_string).collect();

    Ok(AffectedResult { base, changed_count: changed.len(), selection: selection_for(&changed)? })
}

/// The selection a set of changed repository-relative paths produces: the
/// screen for graph-shaping paths first, then the package-graph analysis.
///
/// Split out from [`compute`] because the diff is not the only way to name a
/// changed set — the verify lane asks the same question of a candidate's diff
/// to tell an environment fault from a defect (#4895).
fn selection_for(changed: &[String]) -> Result<Selection> {
    let selection = if changed.is_empty() {
        Selection { run_all: None, packages: BTreeSet::new(), wasm_needed: false }
    } else if let Some(hit) = global_screen(changed) {
        Selection {
            run_all: Some(format!("graph-shaping path changed: {hit}")),
            packages: BTreeSet::new(),
            wasm_needed: true,
        }
    } else {
        Workspace::load()?.select(changed)?
    };

    Ok(selection)
}

/// The workspace packages a set of changed paths can have broken: the packages
/// the paths belong to plus every workspace package that links one of them,
/// transitively. `None` when the change's blast radius is not bounded by the
/// package graph at all — a graph-shaping path, a change that reaches every
/// package, or a changed set nobody could compute.
///
/// The verify lane reads this to tell a failing test that its candidate could
/// have broken from one it could not have (#4895), so `None` is deliberately
/// the *unbounded* answer rather than the empty one: a caller that cannot see
/// the diff must blame the candidate, never the host.
pub fn reverse_dependency_closure(changed: &[String]) -> Result<Option<BTreeSet<String>>> {
    if changed.is_empty() {
        return Ok(None);
    }

    let selection = selection_for(changed)?;
    Ok(selection.run_all.is_none().then_some(selection.packages))
}

fn report(selection: &Selection, changed_count: usize) {
    match &selection.run_all {
        Some(reason) => println!("affected: run everything — {reason}"),
        None if selection.packages.is_empty() => {
            println!("affected: no packages affected by {changed_count} changed path(s) — nothing to test");
        }
        None => {
            let list: Vec<&str> = selection.packages.iter().map(String::as_str).collect();
            println!(
                "affected: {} package(s) from {changed_count} changed path(s), wasm_needed={}: {}",
                list.len(),
                selection.wasm_needed,
                list.join(" "),
            );
        }
    }
}

/// Append the selection to the step-output file CI reads
/// (`steps.<id>.outputs.*`).
fn write_github_output(selection: &Selection) -> Result<()> {
    // CI plumbing, not cap config: GITHUB_OUTPUT is the Actions-provided
    // step-output file path, external to the workspace.
    #[allow(clippy::disallowed_methods)]
    let path = env::var("GITHUB_OUTPUT").context("--github-output needs $GITHUB_OUTPUT (set by GitHub Actions)")?;

    let run_all = selection.run_all.is_some();
    let packages: Vec<&str> = selection.packages.iter().map(String::as_str).collect();
    let package_args: Vec<String> = packages.iter().map(|name| format!("-p {name}")).collect();
    let mut file = fs::OpenOptions::new().append(true).open(&path).with_context(|| format!("open {path}"))?;
    writeln!(file, "run_all={run_all}")?;
    writeln!(file, "wasm_needed={}", selection.wasm_needed)?;
    writeln!(file, "packages={}", packages.join(" "))?;
    writeln!(file, "package_args={}", package_args.join(" "))?;
    Ok(())
}

fn git_stdout(args: &[&str], current_dir: &str) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(current_dir)
        .output()
        .with_context(|| format!("spawn git {args:?}"))?;
    if !output.status.success() {
        bail!("git {args:?} failed ({}): {}", output.status, String::from_utf8_lossy(&output.stderr).trim());
    }
    String::from_utf8(output.stdout).with_context(|| format!("git {args:?} produced non-UTF-8 output"))
}
