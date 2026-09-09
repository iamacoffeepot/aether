//! Shared cargo-invocation layer for the xtask commands: the build
//! profile, the wasm triple, the one `CARGO`-env resolver, the three
//! spawn choke points (`run_status` / `run_captured` /
//! `run_build_denying_warnings`), the `build` command builders, and the
//! artifact/JSON write helpers every command reaches for. Keeping it a
//! single top-level file (versus a command folder) is what makes the tree
//! scan as command folders versus shared files.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::{env, fs};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Serialize;

use crate::inventory::{BuildPlan, CHASSIS_BINS, Component};

/// Wasm triple the components cross-build to.
pub const WASM_TARGET: &str = "wasm32-unknown-unknown";

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    Debug,
    Release,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }

    /// Cargo's profile flag — debug is the default (no flag).
    fn cargo_flag(self) -> Option<&'static str> {
        match self {
            Self::Debug => None,
            Self::Release => Some("--release"),
        }
    }
}

/// A `cargo build` [`Command`] pre-armed with the resolved cargo binary and
/// the profile flag — every build helper starts here and appends its own
/// package / target / bin selectors, so the `CARGO`-env fallback and the
/// profile-flag dance have one home.
pub fn build_command(profile: Profile) -> Command {
    let mut cmd = Command::new(cargo());
    cmd.arg("build");
    if let Some(flag) = profile.cargo_flag() {
        cmd.arg(flag);
    }
    cmd
}

/// Run `cmd` to completion, mirroring its exit status — the status-only
/// spawn choke point. Non-zero is an error tagged with `what`.
pub fn run_status(mut cmd: Command, what: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawn cargo to {what}"))?;
    if !status.success() {
        bail!("cargo failed to {what} ({status})");
    }
    Ok(())
}

/// Run `cmd` to completion, capturing its stdout + stderr — the
/// captured-output spawn choke point, the twin of [`run_status`].
pub fn run_captured(mut cmd: Command) -> Result<Output> {
    cmd.output().context("run command")
}

/// Run a build to completion and fail it on a warning from a workspace crate —
/// the third spawn choke point, for the configurations no other gate compiles.
///
/// The workspace clippy gate runs one feature resolve: cargo unifies features
/// across the members it selects, so every capability's `runtime` half is on
/// and an item that exists only for that half is never dead there. A component
/// cross-build is the opposite resolve — one package at a time, its caps
/// marker-only, on the wasm triple — and that is where a `#[cfg]`-shaped
/// regression shows up as `unused import` / `never constructed`. Nothing was
/// judging that stream, so it accumulated.
///
/// The verdict is derived rather than denied, for the reason `verify.clippy`
/// derives its own: `-D warnings` makes a lint a compile error, so the crate
/// that trips one is never built and everything downstream of it is never
/// compiled at all — the run reports one warning instead of every warning.
/// Asking for `--message-format=json` and judging the complete stream keeps the
/// coverage while applying the same predicate. Cargo replays a fresh unit's
/// cached diagnostics, so a warm cache does not turn the gate green.
///
/// Scoped to `path+` package ids — the tree's own crates. A registry
/// dependency's warning is not this tree's to fix, and denying it would redden
/// the gate on a version bump nobody here wrote.
pub fn run_build_denying_warnings(mut cmd: Command, what: &str) -> Result<()> {
    cmd.arg("--message-format=json").stdout(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| format!("spawn cargo to {what}"))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut warned = BTreeSet::new();

    for line in BufReader::new(stdout).lines() {
        let line = line.with_context(|| format!("read cargo diagnostics while building {what}"))?;
        let Some(diagnostic) = workspace_diagnostic(&line) else {
            continue;
        };

        eprint!("{}", diagnostic.rendered);
        if diagnostic.warning {
            warned.insert(diagnostic.package);
        }
    }

    let status = child.wait().with_context(|| format!("wait for cargo to {what}"))?;
    if !status.success() {
        bail!("cargo failed to {what} ({status})");
    }
    if !warned.is_empty() {
        bail!(
            "{what} warned in {} — the component cross-build is warning-free and stays that way",
            warned.into_iter().collect::<Vec<_>>().join(", "),
        );
    }

    Ok(())
}

/// One rustc diagnostic about a workspace crate, lifted out of cargo's JSON
/// message stream.
struct WorkspaceDiagnostic {
    package: String,
    warning: bool,
    rendered: String,
}

/// Read one cargo `--message-format=json` line as a workspace crate's
/// diagnostic, or `None` for every other message the stream carries.
///
/// `package_id` is what separates the tree's own crates (`path+file:///…`) from
/// its registry dependencies (`registry+https://…`); the level separates a
/// finding from a note the compiler attached to one.
fn workspace_diagnostic(line: &str) -> Option<WorkspaceDiagnostic> {
    let message: serde_json::Value = serde_json::from_str(line).ok()?;
    if message.get("reason")?.as_str()? != "compiler-message" {
        return None;
    }

    let id = message.get("package_id")?.as_str()?;
    if !id.starts_with("path+") {
        return None;
    }

    let level = message.get("message")?.get("level")?.as_str()?;
    Some(WorkspaceDiagnostic {
        package: package_name(id),
        warning: level == "warning",
        rendered: message.get("message")?.get("rendered")?.as_str()?.to_owned(),
    })
}

/// The package name inside a `path+file:///…` id, for the failure line.
///
/// Cargo spells the id two ways: `…/aether-data#0.3.0` when the directory is
/// the package name, and `…/dir#aether-data@0.3.0` when it is not.
fn package_name(id: &str) -> String {
    let (path, fragment) = id.split_once('#').unwrap_or((id, ""));
    match fragment.split_once('@') {
        Some((name, _version)) => name.to_owned(),
        None => path.rsplit('/').next().unwrap_or(path).to_owned(),
    }
}

/// Cargo binary to re-invoke — honours the `CARGO` env var cargo sets for
/// subprocesses, falling back to `cargo` on `PATH`.
// Build tooling: CARGO is the cargo-provided binary path for subprocess
// re-invocation, an external var — xtask is not a capability.
#[allow(clippy::disallowed_methods)]
fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// A command using Cargo's resolved binary, for xtask subcommands that are
/// not builds but must still honor Cargo's own re-invocation environment.
pub fn command() -> Command {
    Command::new(cargo())
}

pub fn build_component(plan: &BuildPlan, profile: Profile) -> Result<()> {
    let mut cmd = build_command(profile);
    cmd.args(["--target", WASM_TARGET, "-p", &plan.package]);
    if plan.examples {
        cmd.arg("--examples");
    }
    if !plan.features.is_empty() {
        cmd.args(["--features", &plan.features.join(",")]);
    }
    let label = if plan.examples {
        format!("{} (examples)", plan.package)
    } else {
        plan.package.clone()
    };
    run_build_denying_warnings(cmd, &format!("build component {label}"))
}

pub fn build_chassis(profile: Profile) -> Result<()> {
    let mut cmd = build_command(profile);
    // One invocation selects every owning package plus every bin —
    // bin selectors are global across the selected packages, and the
    // names are unique workspace-wide.
    let mut packages: Vec<&str> = CHASSIS_BINS.iter().map(|(pkg, _)| *pkg).collect();
    packages.dedup();
    for pkg in packages {
        cmd.args(["-p", pkg]);
    }
    for (_, bin) in CHASSIS_BINS {
        cmd.args(["--bin", bin]);
    }
    run_status(cmd, "build chassis bins")
}

/// Build one chassis binary by `(package, bin)` selector for the host
/// target — the package target's single-bin twin of `build_chassis`'s
/// all-bins build.
pub fn build_named_chassis(package: &str, bin: &str, profile: Profile) -> Result<()> {
    let mut cmd = build_command(profile);
    cmd.args(["-p", package, "--bin", bin]);
    run_status(cmd, &format!("build chassis bin {bin}"))
}

/// Source path of a component's wasm under the target tree. Example
/// cdylibs land under `examples/`; lib cdylibs directly under the profile
/// dir.
pub fn wasm_artifact_path(wasm_profile_dir: &Path, component: &Component) -> PathBuf {
    let file = format!("{}.wasm", component.stem);
    if component.from_example {
        wasm_profile_dir.join("examples").join(file)
    } else {
        wasm_profile_dir.join(file)
    }
}

pub fn copy_artifact(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

/// The chassis binary's on-disk filename for the host platform: cargo appends
/// `.exe` on Windows and leaves the bare name elsewhere. `package` never
/// cross-compiles (no `--target`), so `cfg!(windows)` matches what cargo
/// wrote and what the depot must carry.
pub fn host_binary_filename(bin: &str) -> String {
    if cfg!(windows) {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    }
}

/// Serialize `value` as pretty JSON with a trailing newline and write it to
/// `path` — the one write every manifest / evidence emitter ends on.
pub fn write_json_pretty(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut json = serde_json::to_string_pretty(value).context("serialize json")?;
    json.push('\n');
    fs::write(path, json).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::workspace_diagnostic;

    fn message(package_id: &str, level: &str) -> String {
        format!(
            r#"{{"reason":"compiler-message","package_id":"{package_id}","message":{{"level":"{level}","rendered":"warning: unused import\n"}}}}"#
        )
    }

    #[test]
    fn only_a_workspace_crates_warning_carries_the_verdict() {
        // Tripwire: the gate's whole scope. Reading a registry dependency's
        // warning as a finding would fail the component build on a version
        // bump nobody in this tree wrote; skipping a `path+` one would leave
        // the configuration this gate exists for unjudged.
        let workspace = message("path+file:///repo/crates/aether-data#0.3.0-alpha", "warning");
        let diagnostic = workspace_diagnostic(&workspace).expect("a workspace compiler message is read");
        assert_eq!(diagnostic.package, "aether-data");
        assert!(diagnostic.warning);

        let dependency = message("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228", "warning");
        assert!(workspace_diagnostic(&dependency).is_none(), "a registry crate's warning is not this tree's");

        let note = message("path+file:///repo/crates/aether-data#0.3.0-alpha", "note");
        assert!(!workspace_diagnostic(&note).expect("a note is still rendered").warning);

        assert!(workspace_diagnostic(r#"{"reason":"build-finished","success":true}"#).is_none());
        assert!(workspace_diagnostic("Compiling aether-data v0.3.0-alpha").is_none());
    }

    #[test]
    fn a_directory_that_is_not_the_package_name_still_names_the_package() {
        // Tripwire: cargo spells a path id both ways, and the failure line has
        // to name the crate a reader would go edit.
        let renamed = message("path+file:///repo/crates/kit#aether-kit-widget@0.3.0-alpha", "warning");
        assert_eq!(workspace_diagnostic(&renamed).expect("read").package, "aether-kit-widget");
    }
}
