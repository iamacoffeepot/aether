//! The run a clippy proof asks the workspace for: `cargo fmt`, then
//! `cargo clippy` (ADR-0237 decision 12).
//!
//! Every argument is fixed, so the run key (ADR-0237 decision 9) is the same
//! for every clippy proof in one environment, and they share one allotment
//! estimate and one warm build layer.

use aether_bloomery_kinds::{Ref, Refusal, Tree};
use aether_bloomery_workspace::{EnvVar, Mount, Mounts, Network, RunRequest, Scratch, Step, Steps, ToolName, TreePath};

use super::{ProofBound, refused};

/// The tool every step runs, resolved through the environment's `tools`
/// table. Cargo finds `cargo-fmt` and `cargo-clippy` on the environment's
/// `PATH`.
const TOOL: &str = "cargo";

/// Where the vendor tree is mounted, relative to the root: `/vendor`.
const VENDOR: &str = "vendor";

/// The scratch path that holds cargo's build output.
const TARGET_SCRATCH: &str = "target";

/// The scratch path that holds temporary files and cargo's home.
const TMP_SCRATCH: &str = "tmp";

/// Cargo's home, under [`TMP_SCRATCH`]: the root is read-only.
const CARGO_HOME: &str = "/work/tmp/cargo-home";

/// Cargo's build output, at [`TARGET_SCRATCH`].
const CARGO_TARGET_DIR: &str = "/work/target";

/// Temporary files, at [`TMP_SCRATCH`].
const TMPDIR: &str = "/work/tmp";

/// The variables each run gives every step, over the environment's own.
const ENV: [(&str, &str); 3] = [("CARGO_HOME", CARGO_HOME), ("CARGO_TARGET_DIR", CARGO_TARGET_DIR), ("TMPDIR", TMPDIR)];

/// The first step: format every package of the workspace in place, printing
/// the path of each file rustfmt rewrote, one per line, to stdout. A proof
/// never fails on formatting alone; it fails here only when rustfmt cannot
/// parse a file.
pub(super) const FMT_ARGS: [&str; 4] = ["fmt", "--all", "--", "-l"];

/// The second step: CI's lint command, workspace-wide and `--offline` rather
/// than `--frozen`, so a change to a workspace-internal dependency updates
/// `Cargo.lock` inside the run. Crates.io is replaced by the vendor tree at
/// `/vendor`, `--quiet` keeps cargo's progress lines out of stderr, and the
/// diagnostics come as JSON lines on stdout. The `--config` flags follow the
/// subcommand: `cargo clippy` is an external subcommand that re-runs cargo,
/// and only flags after its name reach that inner cargo.
pub(super) const CLIPPY_ARGS: [&str; 13] = [
    "clippy",
    "--config",
    "source.crates-io.replace-with=\"vendored\"",
    "--config",
    "source.vendored.directory=\"/vendor\"",
    "--workspace",
    "--all-targets",
    "--offline",
    "--quiet",
    "--message-format=json",
    "--",
    "-D",
    "warnings",
];

/// The run that formats and lints `tree` in `bound`'s environment with the
/// network off.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the value a request constructor refused.
pub(super) fn request(tree: Ref<Tree>, bound: &ProofBound) -> Result<RunRequest, Refusal> {
    let vendor = Mount { at: path(VENDOR)?, tree: bound.vendor() };

    Ok(RunRequest {
        tree,
        environment: bound.environment(),
        mounts: Mounts::new(vec![vendor]).map_err(|error| refused(format!("the vendor mount: {error}")))?,
        steps: Steps::new(vec![step(&FMT_ARGS)?, step(&CLIPPY_ARGS)?])
            .map_err(|error| refused(format!("the proof steps: {error}")))?,
        scratch: Scratch::new(vec![path(TARGET_SCRATCH)?, path(TMP_SCRATCH)?])
            .map_err(|error| refused(format!("the scratch paths: {error}")))?,
        network: Network::Off,
    })
}

/// One cargo step with `args` and every variable in [`ENV`].
fn step(args: &[&str]) -> Result<Step, Refusal> {
    Ok(Step {
        tool: ToolName::new(TOOL).map_err(|error| refused(format!("tool {TOOL:?}: {error}")))?,
        args: args.iter().map(|&arg| arg.to_owned()).collect(),
        env: ENV
            .iter()
            .map(|&(key, value)| EnvVar::new(key, value).map_err(|error| refused(format!("variable {key}: {error}"))))
            .collect::<Result<Vec<EnvVar>, Refusal>>()?,
        stdin: None,
    })
}

/// One in-tree path the request spells out itself.
fn path(value: &str) -> Result<TreePath, Refusal> {
    TreePath::new(value).map_err(|error| refused(format!("path {value:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Digest, Ref};
    use aether_bloomery_workspace::Mounts;

    use super::{ENV, ProofBound, request};

    #[test]
    fn every_path_a_step_writes_lies_in_scratch() {
        // Catches a target directory, cargo home, or temp directory that would
        // land in the output tree, or on the read-only root where it fails only live.
        let bound = ProofBound::new(
            Ref::from_digest(Digest::from_bytes([2; 32])),
            Ref::from_digest(Digest::from_bytes([3; 32])),
        );
        let run = request(Ref::from_digest(Digest::from_bytes([1; 32])), &bound).expect("the fixed request builds");

        for step in run.steps.as_slice() {
            for (key, _) in ENV {
                let value = step.env.iter().find(|var| var.key() == key).expect("the step sets the variable").value();
                let in_scratch = run.scratch.as_slice().iter().any(|scratch| {
                    let root = format!("/{}/{}", Mounts::WORK, scratch.as_str());
                    value == root || value.starts_with(&format!("{root}/"))
                });
                assert!(in_scratch, "{key}={value} lies outside every /work/<scratch>");
            }
        }
    }
}
