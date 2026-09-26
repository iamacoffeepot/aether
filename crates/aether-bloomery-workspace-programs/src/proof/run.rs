//! The one-step run a clippy proof asks the workspace for.
//!
//! Every argument is fixed, so the run key (ADR-0237 decision 9) is the same
//! for every clippy proof in one environment, and they share one allotment
//! estimate.

use aether_bloomery_kinds::Refusal;
use aether_workspace::{EnvVar, Mount, Mounts, Network, Run, Scratch, Step, Steps, ToolName, TreePath};

use super::{ClippyInput, refused};

/// The tool the step runs, resolved through the environment's `tools` table.
/// Cargo finds `cargo-clippy` on the environment's `PATH`.
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

/// The variables each run gives the step, over the environment's own.
const ENV: [(&str, &str); 3] = [("CARGO_HOME", CARGO_HOME), ("CARGO_TARGET_DIR", CARGO_TARGET_DIR), ("TMPDIR", TMPDIR)];

/// CI's lint command plus `--frozen` (locked and offline), with crates.io
/// replaced by the vendor tree at `/vendor`. The `--config` flags follow the
/// subcommand: `cargo clippy` is an external subcommand that re-runs cargo,
/// and only flags after its name reach that inner cargo.
const ARGS: [&str; 11] = [
    "clippy",
    "--config",
    "source.crates-io.replace-with=\"vendored\"",
    "--config",
    "source.vendored.directory=\"/vendor\"",
    "--workspace",
    "--all-targets",
    "--frozen",
    "--",
    "-D",
    "warnings",
];

/// The run that lints `input.source` in `input.environment` with the network off.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the value a request constructor refused.
pub(super) fn request(input: &ClippyInput) -> Result<Run, Refusal> {
    let step = Step {
        tool: ToolName::new(TOOL).map_err(|error| refused(format!("tool {TOOL:?}: {error}")))?,
        args: ARGS.iter().map(|&arg| arg.to_owned()).collect(),
        env: ENV
            .iter()
            .map(|&(key, value)| EnvVar::new(key, value).map_err(|error| refused(format!("variable {key}: {error}"))))
            .collect::<Result<Vec<EnvVar>, Refusal>>()?,
        stdin: None,
    };
    let vendor = Mount { at: path(VENDOR)?, tree: input.vendor };

    Ok(Run {
        tree: input.source,
        environment: input.environment,
        mounts: Mounts::new(vec![vendor]).map_err(|error| refused(format!("the vendor mount: {error}")))?,
        steps: Steps::new(vec![step]).map_err(|error| refused(format!("the clippy step: {error}")))?,
        scratch: Scratch::new(vec![path(TARGET_SCRATCH)?, path(TMP_SCRATCH)?])
            .map_err(|error| refused(format!("the scratch paths: {error}")))?,
        network: Network::Off,
    })
}

/// One in-tree path the request spells out itself.
fn path(value: &str) -> Result<TreePath, Refusal> {
    TreePath::new(value).map_err(|error| refused(format!("path {value:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Digest, Ref};
    use aether_workspace::Mounts;

    use super::{ClippyInput, ENV, request};

    #[test]
    fn every_path_the_step_writes_lies_in_scratch() {
        // Catches a target directory, cargo home, or temp directory that would
        // land in the output tree, or on the read-only root where it fails only live.
        let input = ClippyInput {
            source: Ref::from_digest(Digest::from_bytes([1; 32])),
            environment: Ref::from_digest(Digest::from_bytes([2; 32])),
            vendor: Ref::from_digest(Digest::from_bytes([3; 32])),
        };
        let run = request(&input).expect("the fixed request builds");
        let [step] = run.steps.as_slice() else {
            panic!("the request holds one step");
        };

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
