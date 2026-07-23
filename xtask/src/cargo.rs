//! Shared cargo-invocation layer for the xtask commands: the build
//! profile, the wasm triple, the one `CARGO`-env resolver, the two
//! spawn choke points (`run_status` / `run_captured`), the `build`
//! command builders, and the artifact/JSON write helpers every command
//! reaches for. Keeping it a single top-level file (versus a command
//! folder) is what makes the tree scan as command folders versus shared
//! files.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

/// Cargo binary to re-invoke — honours the `CARGO` env var cargo sets for
/// subprocesses, falling back to `cargo` on `PATH`.
// Build tooling: CARGO is the cargo-provided binary path for subprocess
// re-invocation, an external var — xtask is not a capability.
#[allow(clippy::disallowed_methods)]
fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
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
    run_status(cmd, &format!("build component {label}"))
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
