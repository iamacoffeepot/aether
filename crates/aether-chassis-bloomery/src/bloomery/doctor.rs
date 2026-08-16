//! The lane-host tool kit and the doctor that preflights it (#5035).
//!
//! The verify umbrella's `verify.preflight` already refuses a host that cannot
//! run a gate — after the dispatch has been spent. Three instant refusals wedge
//! the member on a host defect no lane retry can fix. This check runs *before*
//! any attempt is recorded: the coordinator logs the kit at boot, the admission
//! gate refuses a dispatch against a missing tool so the member stays queued,
//! and the operator-facing `--doctor` verb runs the same inspect so a host can
//! be validated before a migration, not after the wedge.
//!
//! [`REQUIRED_KIT`] is the one list. Boot, admission, and the doctor verb all
//! read it; adding a gate tool updates all three.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One program the lane host must resolve on the PATH a dispatch inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KitTool {
    /// The executable resolved on `PATH`.
    pub program: &'static str,
    /// What an operator runs to get it — reported verbatim, so a failing
    /// doctor is a fix list rather than a diagnosis exercise.
    pub install: &'static str,
}

/// Every program a dispatched lane may need, in the order an operator reads.
///
/// Mechanical tools first (the ones whose absence is a `verify.preflight`
/// miss), then every model-harness CLI. The list is the contract: boot, the
/// admission gate, and `--doctor` iterate it and nothing else.
pub const REQUIRED_KIT: &[KitTool] = &[
    KitTool { program: "git", install: "install Git (https://git-scm.com)" },
    KitTool { program: "gh", install: "install the GitHub CLI (https://cli.github.com)" },
    KitTool { program: "cargo", install: "install a Rust toolchain via https://rustup.rs" },
    KitTool { program: "rustfmt", install: "rustup component add rustfmt" },
    KitTool { program: "cargo-clippy", install: "rustup component add clippy" },
    KitTool { program: "cargo-nextest", install: "cargo install cargo-nextest --locked" },
    KitTool { program: "cargo-machete", install: "cargo install cargo-machete --locked" },
    KitTool { program: "node", install: "install Node.js (https://nodejs.org)" },
    KitTool { program: "npx", install: "ships with Node.js" },
    KitTool { program: "jscpd", install: "npm install -g jscpd" },
    KitTool { program: "python3", install: "install Python 3 (https://www.python.org)" },
    KitTool { program: "claude", install: "install the Claude Code CLI" },
    KitTool { program: "codex", install: "install the Codex CLI" },
    KitTool { program: "muse", install: "install the Muse Code CLI" },
    KitTool { program: "grok", install: "install the Grok Build CLI" },
];

/// A kit program that resolved and answered `--version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    /// Absolute (or PATH-joined) path the probe executed.
    pub path: PathBuf,
    /// The first line of `--version` stdout, trimmed.
    pub version: String,
}

/// One row of a kit inspect: present with a path and version, or missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStatus {
    /// The program [`REQUIRED_KIT`] named.
    pub program: &'static str,
    /// How to install it, from the same row.
    pub install: &'static str,
    /// `Some` when the program resolved on the consulted PATH and `--version`
    /// succeeded; `None` when it did not.
    pub resolved: Option<ResolvedTool>,
}

impl ToolStatus {
    /// Whether this row is a missing tool.
    #[must_use]
    pub fn is_missing(&self) -> bool {
        self.resolved.is_none()
    }
}

/// The result of inspecting the kit against one PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KitReport {
    /// The PATH that was consulted, or empty when the process has none.
    pub path: OsString,
    /// One row per [`REQUIRED_KIT`] entry, in that order.
    pub tools: Vec<ToolStatus>,
}

impl KitReport {
    /// Inspect the kit against the process PATH — the same PATH a dispatched
    /// lane inherits (coordinator env is scrubbed of `AETHER_*` knobs, not of
    /// `PATH`; see `lane_env`).
    #[must_use]
    pub fn inspect() -> Self {
        // PATH is the process search path a dispatched lane inherits, not a
        // derive-Config knob. `var_os` is the disallowed config read; walk
        // `vars_os` the way `lane_env` already enumerates the environment.
        let path = env::vars_os().find_map(|(key, value)| (key == "PATH").then_some(value));
        Self::inspect_on(path)
    }

    /// Inspect the kit against an explicit PATH, so a test can name a directory
    /// of stand-in binaries without mutating the process environment.
    #[must_use]
    pub fn inspect_on(path: Option<OsString>) -> Self {
        let path = path.unwrap_or_default();
        let tools = REQUIRED_KIT.iter().map(|tool| probe(tool, &path)).collect();
        Self { path, tools }
    }

    /// Every kit tool this host does not have.
    pub fn missing(&self) -> impl Iterator<Item = &ToolStatus> {
        self.tools.iter().filter(|tool| tool.is_missing())
    }

    /// Whether every required tool resolved.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.tools.iter().all(|tool| !tool.is_missing())
    }

    /// The PATH as an operator reads it — `"(unset)"` when the process has none,
    /// so a missing-tool warning always names what was consulted.
    #[must_use]
    pub fn path_display(&self) -> String {
        if self.path.is_empty() {
            return String::from("(unset)");
        }
        self.path.to_string_lossy().into_owned()
    }

    /// Operator-facing doctor text: every tool's path and version, or its
    /// install line when missing, plus a one-line verdict.
    #[must_use]
    pub fn render_doctor(&self) -> String {
        let mut lines = vec![format!("lane PATH: {}", self.path_display()), String::new()];
        for tool in &self.tools {
            match &tool.resolved {
                Some(resolved) => {
                    lines.push(format!("{:<16} {}  {}", tool.program, resolved.path.display(), resolved.version));
                }
                None => lines.push(format!("{:<16} MISSING  {}", tool.program, tool.install)),
            }
        }
        lines.push(String::new());
        if self.is_ready() {
            lines.push(String::from("lane host kit is complete"));
        } else {
            lines.push(format!("lane host kit is incomplete; missing: {}", self.missing_names()));
        }
        lines.push(String::new());
        lines.join("\n")
    }

    /// The admission-gate refusal: the same missing-tool list the doctor prints,
    /// named as a dispatch refusal so the member stays queued.
    #[must_use]
    pub fn render_refusal(&self) -> Option<String> {
        let missing: Vec<&ToolStatus> = self.missing().collect();
        if missing.is_empty() {
            return None;
        }
        let list = missing
            .iter()
            .map(|tool| format!("- `{}` — {}", tool.program, tool.install))
            .collect::<Vec<String>>()
            .join("\n");
        Some(format!(
            "lane host is missing kit tools on PATH `{}`:\n{list}\n\
             install these on the executor host; the member stays queued and no attempt is spent",
            self.path_display()
        ))
    }

    /// Log every resolved tool at info and every missing one as a loud warning
    /// that names the PATH consulted — the coordinator-boot face of the same
    /// inspect `--doctor` and the admission gate run.
    pub fn log_at_boot(&self) {
        for tool in &self.tools {
            if let Some(resolved) = &tool.resolved {
                tracing::info!(
                    target: "aether_chassis_bloomery::doctor",
                    program = tool.program,
                    path = %resolved.path.display(),
                    version = %resolved.version,
                    "lane host kit tool",
                );
            } else {
                tracing::warn!(
                    target: "aether_chassis_bloomery::doctor",
                    program = tool.program,
                    install = tool.install,
                    path = %self.path_display(),
                    "lane host kit is missing a required tool on the lane PATH",
                );
            }
        }
        if !self.is_ready() {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                missing = %self.missing_names(),
                path = %self.path_display(),
                "lane host kit is incomplete; local dispatches will be refused until the missing tools resolve",
            );
        }
    }

    fn missing_names(&self) -> String {
        self.missing().map(|tool| tool.program).collect::<Vec<_>>().join(", ")
    }
}

/// Probe `tool` against `path`: resolve the binary, run `--version`, and treat
/// anything that does not answer as missing.
fn probe(tool: &KitTool, path: &OsStr) -> ToolStatus {
    let resolved = resolve_on_path(tool.program, path)
        .and_then(|resolved| version_of(&resolved).map(|version| ResolvedTool { path: resolved, version }));
    ToolStatus { program: tool.program, install: tool.install, resolved }
}

/// The first runnable `program` on `path`, or `None` when nothing matches.
fn resolve_on_path(program: &str, path: &OsStr) -> Option<PathBuf> {
    env::split_paths(path).map(|dir| dir.join(program)).find(|candidate| is_runnable(candidate))
}

fn is_runnable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn version_of(program: &Path) -> Option<String> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.lines().next().map(str::trim).filter(|line| !line.is_empty())?;
    Some(version.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use aether_bloomery::Harness;

    use super::{KitReport, REQUIRED_KIT, ResolvedTool, ToolStatus, resolve_on_path, version_of};

    const MODEL_HARNESSES: [Harness; 4] = [Harness::Claude, Harness::Codex, Harness::Muse, Harness::Grok];

    /// A directory whose only runnable program is `git`, answering `--version`
    /// with a known line — the stand-in PATH a missing-tool case inspects.
    fn path_with_only_git() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        write_versioned(dir.path(), "git", "git version 2.0-test");
        let path = dir.path().display().to_string();
        (dir, path)
    }

    fn write_versioned(dir: &Path, name: &str, version_line: &str) {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }

    #[test]
    fn a_missing_tool_is_named_the_same_by_the_doctor_and_the_admission_refusal() {
        // Tripwire: boot, `--doctor`, and the submit gate must read one list.
        // A doctor that reports `jscpd` while admission refuses something else
        // — or a refusal that stays silent — is how a host defect spends three
        // attempts and wedges (#5035).
        let (dir, path) = path_with_only_git();
        let report = KitReport::inspect_on(Some(path.clone().into()));

        let missing: Vec<&str> = report.missing().map(|tool| tool.program).collect();
        assert!(missing.contains(&"jscpd"), "jscpd is a kit tool and is absent from this PATH");
        assert!(!missing.contains(&"git"), "git resolved on the stand-in PATH");
        assert_eq!(
            report.tools.iter().find(|tool| tool.program == "git").and_then(|tool| tool.resolved.as_ref()),
            Some(&ResolvedTool { path: dir.path().join("git"), version: "git version 2.0-test".to_owned() }),
            "a present tool carries the path and version the probe ran",
        );

        let doctor = report.render_doctor();
        let refusal = report.render_refusal().expect("a PATH missing kit tools must refuse");
        assert!(doctor.contains("jscpd"), "the doctor names the missing tool");
        assert!(doctor.contains("MISSING"), "and marks it missing");
        assert!(doctor.contains(&path), "and names the PATH it consulted");
        assert!(refusal.contains("`jscpd`"), "the admission refusal names the same tool");
        assert!(refusal.contains("npm install -g jscpd"), "and the same install line");
        assert!(refusal.contains(&path), "and the same PATH");
        assert!(refusal.contains("stays queued"), "the refusal must say the member is not spending an attempt");
        assert!(
            REQUIRED_KIT
                .iter()
                .filter(|tool| tool.program != "git")
                .all(|tool| { doctor.contains(tool.program) && refusal.contains(&format!("`{}`", tool.program)) }),
            "every missing kit tool appears in both renderings: doctor={doctor} refusal={refusal}",
        );
    }

    #[test]
    fn a_complete_kit_passes_the_doctor_and_carries_no_refusal() {
        // The other half of the acceptance: a host that has the kit must not
        // invent a refusal. Building a stand-in for every row (not the process
        // PATH) is what makes the pass independent of whatever this machine
        // happens to have installed.
        let dir = tempfile::tempdir().unwrap();
        for tool in REQUIRED_KIT {
            write_versioned(dir.path(), tool.program, &format!("{} 1.0-test", tool.program));
        }
        let report = KitReport::inspect_on(Some(dir.path().as_os_str().to_owned()));

        assert!(report.is_ready(), "every kit tool resolved: {:?}", report.missing().collect::<Vec<_>>());
        assert!(report.render_refusal().is_none(), "a complete kit must not refuse dispatch");
        assert!(
            report.render_doctor().contains("lane host kit is complete"),
            "the doctor verdict is a pass, not a silent empty body",
        );
    }

    #[test]
    fn every_model_harness_cli_is_in_the_kit() {
        // Tripwire: a new `Harness` arm is a new lane CLI. Leaving it off the
        // kit lets a sealed bloom dispatch a model the host cannot spawn, and
        // the miss is only discovered after the attempt is spent.
        for harness in MODEL_HARNESSES {
            assert!(
                REQUIRED_KIT.iter().any(|tool| tool.program == harness.as_str()),
                "REQUIRED_KIT must name the `{}` CLI; adding a Harness without a kit row drops it from doctor and admission",
                harness.as_str(),
            );
        }
    }

    #[test]
    fn an_unset_path_names_itself_in_the_refusal() {
        // Tripwire: a missing-tool warning that does not name the PATH it
        // consulted sends the operator to the wrong place — they install the
        // tool on a PATH the lane will never see.
        let report = KitReport::inspect_on(None);
        let refusal = report.render_refusal().expect("an unset PATH has no kit tools");
        assert!(refusal.contains("(unset)"), "the refusal must say the lane PATH was unset: {refusal}");
        assert!(report.tools.iter().all(ToolStatus::is_missing));
    }

    #[test]
    fn a_non_executable_file_is_not_a_tool() {
        // A PATH entry that exists but cannot run is the same defect as a
        // missing one: `verify.preflight` would still refuse, and treating the
        // file as present would let admission spend an attempt on a host that
        // cannot spawn the program.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jscpd");
        fs::write(&path, "not executable\n").unwrap();

        assert!(resolve_on_path("jscpd", dir.path().as_os_str()).is_none());
    }

    #[test]
    fn a_binary_that_rejects_version_is_not_present() {
        // `--version` is the probe, matching the verify preflight. A file that
        // resolves but exits non-zero is not a tool the lane can run.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jscpd");
        fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();

        assert!(version_of(&path).is_none(), "a failing --version is absence, not a present tool with no version");
    }
}
