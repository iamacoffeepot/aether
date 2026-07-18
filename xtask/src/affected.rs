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
//! [`CHASSIS_PACKAGE`], whose scenario tests execute that crate's wasm.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::process::Command;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use clap::Args;
use determinator::Determinator;
use determinator::rules::DeterminatorRules;
use guppy::graph::{DependencyDirection, PackageGraph};

use crate::inventory::{CHASSIS_PACKAGE, discover_behaviors, discover_components};

/// Paths whose change invalidates the selection premise: they shape the
/// dependency graph, the toolchain, the test configuration, or the
/// selection machinery itself. Any hit forces `run_all` before the
/// package-graph analysis runs — which is also what makes the
/// same-graph-twice determinator call in [`select`] sound: a path that
/// could change the graph never reaches it.
const RUN_ALL_EXACT: &[&str] =
    &["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "clippy.toml", ".github/workflows/ci.yml"];

/// Directory prefixes with the same run-everything force as
/// [`RUN_ALL_EXACT`]: cargo config, nextest config, and this tool's own
/// crate.
const RUN_ALL_PREFIXES: &[&str] = &[".cargo/", ".config/", "xtask/"];

/// Custom determinator path rules, applied before the crate's bundled
/// defaults (which already ignore `README*` / `LICENSE*` / `.gitignore`
/// and mark-all on the root manifest).
///
/// The ignore list is paths that provably cannot change a Rust build or
/// test outcome: prose, agent/pipeline state, non-`ci.yml` workflows
/// (`ci.yml` itself is screened to `run_all` before rules run), and the
/// `fuzz/` tree, which is its own cargo workspace built only by
/// fuzz-nightly. `bloomery/**` is the opposite case — a cross-boundary
/// test input: the `aether-bloomery-host` approve tests read
/// `bloomery/approval-policy.yml` from the repo root, so a change there
/// marks that package (and its reverse closure) changed.
const PATH_RULES_TOML: &str = r#"
[[path-rule]]
globs = ["docs/**", "scripts/**", ".claude/**", ".agents/**", ".codex/**", ".github/**", "fuzz/**", ".jscpd.json", ".mcp.json", "CLAUDE.md", "AGENTS.md"]
mark-changed = []

[[path-rule]]
globs = ["bloomery/**"]
mark-changed = ["aether-bloomery-host"]
"#;

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
}

/// The computed test selection.
struct Selection {
    /// `Some(reason)` when the whole workspace suite must run.
    run_all: Option<String>,
    /// Affected workspace package names (empty when `run_all` is set —
    /// the full invocation ignores the list).
    packages: BTreeSet<String>,
    /// Whether the `cargo xtask dist` wasm pre-build is needed before
    /// the tests run — see [`derive_wasm_needed`].
    wasm_needed: bool,
}

pub fn run(args: &AffectedArgs) -> Result<()> {
    let repo_root = git_stdout(&["rev-parse", "--show-toplevel"], ".")?;
    let repo_root = repo_root.trim();
    let base = match &args.base {
        Some(base) => base.clone(),
        None => git_stdout(&["merge-base", "origin/main", "HEAD"], repo_root)?.trim().to_string(),
    };

    let diff = git_stdout(&["diff", "--name-only", "-z", &base, "HEAD"], repo_root)?;
    let changed: Vec<String> = diff.split('\0').filter(|path| !path.is_empty()).map(str::to_string).collect();

    let selection = if changed.is_empty() {
        Selection { run_all: None, packages: BTreeSet::new(), wasm_needed: false }
    } else if let Some(hit) = global_screen(&changed) {
        Selection {
            run_all: Some(format!("graph-shaping path changed: {hit}")),
            packages: BTreeSet::new(),
            wasm_needed: true,
        }
    } else {
        let graph = guppy::MetadataCommand::new().build_graph().context("build guppy package graph")?;
        let metadata =
            cargo_metadata::MetadataCommand::new().no_deps().exec().context("run cargo metadata for inventory")?;
        let wasm_sources: BTreeSet<String> = discover_components(&metadata)
            .into_iter()
            .map(|component| component.package)
            .chain(discover_behaviors(&metadata).into_iter().map(|behavior| behavior.package))
            .collect();
        let wasm_consumers: BTreeSet<String> = metadata
            .packages
            .iter()
            .filter(|package| {
                package.dependencies.iter().any(|dependency| wasm_sources.contains(dependency.name.as_str()))
            })
            .map(|package| package.name.to_string())
            .collect();
        select(&graph, &changed, &wasm_sources, &wasm_consumers)?
    };

    report(&selection, changed.len());
    if args.github_output {
        write_github_output(&selection)?;
    }
    Ok(())
}

/// Screen for paths that force the full suite, returning the first hit.
fn global_screen(changed: &[String]) -> Option<&str> {
    changed
        .iter()
        .map(String::as_str)
        .find(|path| RUN_ALL_EXACT.contains(path) || RUN_ALL_PREFIXES.iter().any(|prefix| path.starts_with(prefix)))
}

/// Map changed paths onto the package graph and take the
/// reverse-dependency closure, then inject the wasm runtime coupling.
///
/// The same graph is passed as determinator's old and new state: its
/// dual-graph analysis exists to catch manifest reshapes, and every path
/// that could reshape the graph is already screened to `run_all` by
/// [`global_screen`] (a member-crate dependency edit always touches
/// `Cargo.lock`).
fn select(
    graph: &PackageGraph,
    changed: &[String],
    wasm_sources: &BTreeSet<String>,
    wasm_consumers: &BTreeSet<String>,
) -> Result<Selection> {
    let rules = DeterminatorRules::parse(PATH_RULES_TOML).context("parse built-in determinator path rules")?;
    let mut determinator = Determinator::new(graph, graph);
    determinator.set_rules(&rules).context("apply determinator path rules")?;
    determinator.add_changed_paths(changed.iter().map(String::as_str));

    let affected = determinator.compute().affected_set;
    let mut packages: BTreeSet<String> = affected
        .packages(DependencyDirection::Forward)
        .filter(guppy::graph::PackageMetadata::in_workspace)
        .map(|package| package.name().to_string())
        .collect();
    if packages.len() == graph.workspace().iter().count() {
        return Ok(Selection {
            run_all: Some("every workspace package is affected".to_string()),
            packages: BTreeSet::new(),
            wasm_needed: true,
        });
    }

    inject_wasm_coupling(&mut packages, wasm_sources);
    let wasm_needed = derive_wasm_needed(&packages, wasm_sources, wasm_consumers);
    Ok(Selection { run_all: None, packages, wasm_needed })
}

/// Whether the `cargo xtask dist` wasm pre-build must run before the
/// selected tests: the chassis package's scenario tests execute component
/// wasm, a wasm-source crate's own tests may read its wasm, and a crate
/// that depends on a wasm source can execute that source's wasm at test
/// time (issue #3617 — `aether-bloomery-host`'s control-loop tests run
/// `aether-bloomery`'s control-core wasm and hard-fail under
/// `AETHER_REQUIRE_RUNTIME` when it was not pre-built).
fn derive_wasm_needed(
    packages: &BTreeSet<String>,
    wasm_sources: &BTreeSet<String>,
    wasm_consumers: &BTreeSet<String>,
) -> bool {
    packages.contains(CHASSIS_PACKAGE)
        || packages.iter().any(|name| wasm_sources.contains(name) || wasm_consumers.contains(name))
}

/// The coupling cargo's graph cannot see: [`CHASSIS_PACKAGE`]'s scenario
/// tests execute the wasm built from component and behavior crates, so an
/// affected wasm-source crate pulls the chassis package into the
/// selection.
fn inject_wasm_coupling(packages: &mut BTreeSet<String>, wasm_sources: &BTreeSet<String>) {
    if packages.iter().any(|name| wasm_sources.contains(name)) {
        packages.insert(CHASSIS_PACKAGE.to_string());
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn string_set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn global_screen_catches_graph_shaping_paths() {
        // A missed screen entry would let a graph-reshaping or
        // config-reshaping change run a stale subset.
        for path in [
            "Cargo.lock",
            "Cargo.toml",
            "rust-toolchain.toml",
            ".config/nextest.toml",
            ".cargo/config.toml",
            "xtask/src/affected.rs",
            ".github/workflows/ci.yml",
        ] {
            assert!(global_screen(&strings(&[path])).is_some(), "{path} must force run_all");
        }

        for path in ["crates/aether-kit/src/lib.rs", "crates/aether-kit/Cargo.toml", "docs/guide/testing.md"] {
            assert!(global_screen(&strings(&[path])).is_none(), "{path} must not force run_all");
        }
    }

    #[test]
    fn wasm_coupling_injects_chassis_package() {
        // A forgotten injection silently deselects the scenario tests
        // that execute a changed component's wasm.
        let wasm_sources = string_set(&["aether-kit"]);
        let mut with_component = string_set(&["aether-kit"]);
        inject_wasm_coupling(&mut with_component, &wasm_sources);
        assert!(with_component.contains(CHASSIS_PACKAGE), "component change must pull in {CHASSIS_PACKAGE}");

        let mut without_component = string_set(&["aether-bloomery-host"]);
        inject_wasm_coupling(&mut without_component, &wasm_sources);
        assert!(
            !without_component.contains(CHASSIS_PACKAGE),
            "non-component change must not pull in {CHASSIS_PACKAGE}"
        );
    }

    #[test]
    fn wasm_needed_covers_consumers_beyond_the_chassis() {
        // The canary shape from issue #3617: aether-bloomery-host is not
        // the chassis and not a wasm source, but its tests execute
        // aether-bloomery's control-core wasm — deriving wasm_needed from
        // chassis membership alone skipped the dist pre-build and
        // hard-failed under AETHER_REQUIRE_RUNTIME.
        let wasm_sources = string_set(&["aether-bloomery"]);
        let wasm_consumers = string_set(&["aether-bloomery-host"]);
        assert!(
            derive_wasm_needed(&string_set(&["aether-bloomery-host"]), &wasm_sources, &wasm_consumers),
            "a selected wasm-consumer crate needs the pre-build"
        );
        assert!(
            derive_wasm_needed(&string_set(&["aether-bloomery"]), &wasm_sources, &wasm_consumers),
            "a selected wasm-source crate needs the pre-build"
        );
        assert!(
            !derive_wasm_needed(&string_set(&["aether-math"]), &wasm_sources, &wasm_consumers),
            "a crate with no wasm relationship must not force the pre-build"
        );
    }

    #[test]
    fn real_graph_closure_and_conservative_fallback() {
        let graph = guppy::MetadataCommand::new().build_graph().expect("build package graph");
        let no_wasm_sources = BTreeSet::new();
        let no_wasm_consumers = BTreeSet::new();

        // A leaf-crate change selects that crate but not the chassis
        // package — the payoff case this tool exists for. An inverted or
        // over-wide closure shows up here.
        let leaf =
            select(&graph, &strings(&["crates/aether-bloomery-host/src/lib.rs"]), &no_wasm_sources, &no_wasm_consumers)
                .expect("select over leaf change");
        assert!(leaf.run_all.is_none(), "leaf change must not run everything");
        assert!(leaf.packages.contains("aether-bloomery-host"), "changed crate must be selected");
        assert!(!leaf.packages.contains(CHASSIS_PACKAGE), "unrelated chassis package must not be selected");

        // A path matching no package and no rule must fall back to the
        // whole workspace — silent deselection of unknown inputs is the
        // one failure mode this tool must never have.
        let unknown = select(&graph, &strings(&["mystery-toplevel-input.txt"]), &no_wasm_sources, &no_wasm_consumers)
            .expect("select over unknown path");
        assert!(unknown.run_all.is_some(), "unknown path must run everything");

        // The bloomery/** rule maps the cross-boundary test input to its
        // reader instead of falling back to run-everything.
        let policy = select(&graph, &strings(&["bloomery/approval-policy.yml"]), &no_wasm_sources, &no_wasm_consumers)
            .expect("select over bloomery config change");
        assert!(policy.run_all.is_none(), "bloomery config maps to a package, not run_all");
        assert!(policy.packages.contains("aether-bloomery-host"), "bloomery config change must select its reader");
    }
}
