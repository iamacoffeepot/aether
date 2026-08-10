//! Execution of the test command selected by `cargo xtask affected --run`.

use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::affected::select::Selection;
use crate::cargo::{self, WASM_TARGET};

const REQUIRED_ENV: &[(&str, &str)] = &[("AETHER_REQUIRE_RUNTIME", "1"), ("AETHER_STORE_PATH", ":memory:")];

#[derive(Debug, PartialEq, Eq)]
struct CommandPlan {
    args: Vec<String>,
    env: Vec<(&'static str, &'static str)>,
}

#[derive(Debug, PartialEq, Eq)]
enum Prerequisite {
    Nextest,
    WasmTarget,
}

#[derive(Debug, PartialEq, Eq)]
struct RunPlan {
    prerequisites: Vec<Prerequisite>,
    commands: Vec<CommandPlan>,
}

pub(super) fn execute(selection: &Selection) -> Result<()> {
    let plan = RunPlan::for_selection(selection);
    if plan.commands.is_empty() {
        return Ok(());
    }

    for prerequisite in &plan.prerequisites {
        prerequisite.check()?;
    }

    run_commands(&plan, run_command)
}

impl Prerequisite {
    fn check(&self) -> Result<()> {
        match self {
            Self::Nextest => require_nextest(),
            Self::WasmTarget => require_wasm_target(),
        }
    }
}

impl RunPlan {
    fn for_selection(selection: &Selection) -> Self {
        if selection.run_all.is_some() {
            return Self {
                prerequisites: vec![Prerequisite::Nextest, Prerequisite::WasmTarget],
                commands: vec![dist(), workspace_tests()],
            };
        }
        if selection.packages.is_empty() {
            return Self { prerequisites: Vec::new(), commands: Vec::new() };
        }

        let mut commands = vec![xtask_invariants()];
        if selection.wasm_needed {
            commands.push(dist());
        }
        commands.push(affected_tests(selection));
        let mut prerequisites = vec![Prerequisite::Nextest];
        if selection.wasm_needed {
            prerequisites.push(Prerequisite::WasmTarget);
        }
        Self { prerequisites, commands }
    }
}

fn xtask_invariants() -> CommandPlan {
    CommandPlan { args: strings(&["nextest", "run", "-p", "xtask", "--profile", "ci"]), env: Vec::new() }
}

fn dist() -> CommandPlan {
    CommandPlan { args: strings(&["xtask", "dist"]), env: Vec::new() }
}

fn workspace_tests() -> CommandPlan {
    CommandPlan {
        args: strings(&["nextest", "run", "--all-features", "--profile", "ci", "--partition", "slice:1/1"]),
        env: REQUIRED_ENV.to_vec(),
    }
}

fn affected_tests(selection: &Selection) -> CommandPlan {
    let mut args = strings(&["nextest", "run"]);
    for package in &selection.packages {
        args.extend(["-p".to_string(), package.clone()]);
    }
    args.extend(strings(&["--all-features", "--profile", "ci"]));
    CommandPlan { args, env: REQUIRED_ENV.to_vec() }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn require_nextest() -> Result<()> {
    let status = cargo::command().args(["nextest", "--version"]).status().context("check for cargo-nextest")?;
    if !status.success() {
        bail!("cargo-nextest is required for `cargo xtask affected --run`; install it before running tests");
    }
    Ok(())
}

fn require_wasm_target() -> Result<()> {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .context("check installed Rust targets")?;
    if !output.status.success() {
        bail!("could not inspect installed Rust targets before running `cargo xtask dist`");
    }
    if !String::from_utf8_lossy(&output.stdout).lines().any(|target| target == WASM_TARGET) {
        bail!("{WASM_TARGET} is required for `cargo xtask affected --run`; install it before running tests");
    }
    Ok(())
}

fn run_commands(plan: &RunPlan, mut run_command: impl FnMut(&CommandPlan) -> Result<()>) -> Result<()> {
    for command in &plan.commands {
        run_command(command)?;
    }
    Ok(())
}

fn run_command(plan: &CommandPlan) -> Result<()> {
    let label = format!("cargo {}", plan.args.join(" "));
    let status = cargo::command()
        .args(&plan.args)
        .envs(plan.env.iter().copied())
        .status()
        .with_context(|| format!("spawn {label}"))?;
    if !status.success() {
        bail!("{label} failed ({status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use anyhow::bail;

    use super::{Prerequisite, REQUIRED_ENV, RunPlan, run_commands, strings};
    use crate::affected::select::Selection;

    fn selection(run_all: Option<&str>, packages: &[&str], wasm_needed: bool) -> Selection {
        Selection {
            run_all: run_all.map(str::to_string),
            packages: packages.iter().map(|package| (*package).to_string()).collect::<BTreeSet<_>>(),
            wasm_needed,
        }
    }

    #[test]
    fn empty_selection_runs_nothing() {
        assert!(RunPlan::for_selection(&selection(None, &[], false)).commands.is_empty());
    }

    #[test]
    fn narrowed_selection_runs_invariants_then_sorted_packages() {
        let plan = RunPlan::for_selection(&selection(None, &["zeta", "alpha"], false));
        assert_eq!(plan.prerequisites, vec![Prerequisite::Nextest]);
        assert_eq!(plan.commands.len(), 2);
        assert_eq!(plan.commands[0].args, strings(&["nextest", "run", "-p", "xtask", "--profile", "ci"]));
        assert_eq!(
            plan.commands[1].args,
            strings(&["nextest", "run", "-p", "alpha", "-p", "zeta", "--all-features", "--profile", "ci"])
        );
        assert_eq!(plan.commands[1].env, REQUIRED_ENV);
    }

    #[test]
    fn wasm_consumer_prebuilds_before_selected_tests() {
        let plan = RunPlan::for_selection(&selection(None, &["aether-chassis"], true));
        assert_eq!(plan.prerequisites, vec![Prerequisite::Nextest, Prerequisite::WasmTarget]);
        assert_eq!(plan.commands[1].args, strings(&["xtask", "dist"]));
        assert_eq!(plan.commands[2].env, REQUIRED_ENV);
    }

    #[test]
    fn run_all_prebuilds_then_runs_the_one_shard_workspace_equivalent() {
        let plan = RunPlan::for_selection(&selection(Some("changed CI"), &[], true));
        assert_eq!(plan.prerequisites, vec![Prerequisite::Nextest, Prerequisite::WasmTarget]);
        assert_eq!(plan.commands[0].args, strings(&["xtask", "dist"]));
        assert_eq!(
            plan.commands[1].args,
            strings(&["nextest", "run", "--all-features", "--profile", "ci", "--partition", "slice:1/1"])
        );
        assert_eq!(plan.commands[1].env, REQUIRED_ENV);
    }

    #[test]
    fn command_failure_stops_the_remaining_plan() {
        let plan = RunPlan::for_selection(&selection(None, &["aether-chassis"], true));
        let mut started = 0;
        let error = run_commands(&plan, |_| {
            started += 1;
            bail!("first command failed")
        })
        .expect_err("the first command failure propagates");
        assert_eq!(started, 1);
        assert!(error.to_string().contains("first command failed"));
    }
}
