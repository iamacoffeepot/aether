//! The run a proof asks the workspace for: `cargo fmt`, then a cargo step
//! (ADR-0237 decision 12).
//!
//! Every argument is fixed, so the run key (ADR-0237 decision 9) is the same
//! for every proof of one kind in one environment, and they share one
//! allotment estimate and one warm build layer. The test step also takes the
//! bound's test env, so its key varies only with the session's env.

use aether_bloomery_kinds::{Ref, Refusal, Tree};
use aether_bloomery_workspace::{EnvVar, Mount, Mounts, Network, RunRequest, Scratch, Step, Steps, ToolName, TreePath};

use super::{ProofBound, refused};

/// The tool every step runs, resolved through the environment's `tools`
/// table. Cargo finds `cargo-fmt` and `cargo-clippy` on the environment's
/// `PATH`.
const TOOL: &str = "cargo";

/// Where the vendor tree is mounted, relative to the root: `/vendor`.
pub(super) const VENDOR: &str = "vendor";

/// Where the cargo config tree is mounted, relative to the root: `/.cargo`,
/// an ancestor of every step's working directory `/work`.
const CARGO_CONFIG: &str = ".cargo";

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

/// The clippy step: CI's lint command, workspace-wide and `--offline` rather
/// than `--frozen`, so a change to a workspace-internal dependency updates
/// `Cargo.lock` inside the run. Crates.io is replaced by the vendor tree
/// through the config at `/.cargo`, `--quiet` keeps cargo's progress lines
/// out of stderr, and the diagnostics come as JSON lines on stdout.
pub(super) const CLIPPY_ARGS: [&str; 9] =
    ["clippy", "--workspace", "--all-targets", "--offline", "--quiet", "--message-format=json", "--", "-D", "warnings"];

/// The test step: the workspace tests with cargo's default target selection
/// (lib, bins, tests, doctests), `--offline` rather than `--frozen` like
/// clippy's, with crates.io replaced by the config at `/.cargo`.
/// `--no-fail-fast` reports every failing target, not only the first, and
/// `--quiet` keeps cargo's status lines out of stderr.
pub(super) const TEST_ARGS: [&str; 6] =
    ["test", "--workspace", "--offline", "--quiet", "--no-fail-fast", "--message-format=json"];

/// The run that formats `tree` in `bound`'s environment with the network
/// off, then runs the cargo step `cargo` with `extra` appended to its
/// variables. Only the cargo step takes `extra`, so the fmt step, and with an
/// empty `extra` the whole request, is the same whatever the bound's test env
/// is.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the value a request constructor refused.
pub(super) fn request(
    tree: Ref<Tree>,
    bound: &ProofBound,
    cargo: &[&str],
    extra: &[EnvVar],
) -> Result<RunRequest, Refusal> {
    let vendor = Mount { at: path(VENDOR)?, tree: bound.vendor() };
    let cargo_config = Mount { at: path(CARGO_CONFIG)?, tree: bound.cargo_config() };

    Ok(RunRequest {
        tree,
        environment: bound.environment(),
        mounts: Mounts::new(vec![vendor, cargo_config])
            .map_err(|error| refused(format!("the proof mounts: {error}")))?,
        steps: Steps::new(vec![step(&FMT_ARGS, &[])?, step(cargo, extra)?])
            .map_err(|error| refused(format!("the proof steps: {error}")))?,
        scratch: Scratch::new(vec![path(TARGET_SCRATCH)?, path(TMP_SCRATCH)?])
            .map_err(|error| refused(format!("the scratch paths: {error}")))?,
        network: Network::Off,
    })
}

/// One cargo step with `args`, every variable in [`ENV`], and `extra`
/// appended after them.
fn step(args: &[&str], extra: &[EnvVar]) -> Result<Step, Refusal> {
    Ok(Step {
        tool: ToolName::new(TOOL).map_err(|error| refused(format!("tool {TOOL:?}: {error}")))?,
        args: args.iter().map(|&arg| arg.to_owned()).collect(),
        env: ENV
            .iter()
            .map(|&(key, value)| EnvVar::new(key, value).map_err(|error| refused(format!("variable {key}: {error}"))))
            .chain(extra.iter().cloned().map(Ok))
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
    use aether_bloomery_workspace::{EnvVar, Mounts};

    use super::{CLIPPY_ARGS, ENV, TEST_ARGS, request};
    use crate::proof::{ProofBound, TestEnv};

    fn bound() -> ProofBound {
        ProofBound::new(
            Ref::from_digest(Digest::from_bytes([2; 32])),
            Ref::from_digest(Digest::from_bytes([3; 32])),
            TestEnv::default(),
        )
    }

    #[test]
    fn every_path_a_step_writes_lies_in_scratch() {
        // Catches a target directory, cargo home, or temp directory that would
        // land in the output tree, or on the read-only root where it fails only live.
        let tree = Ref::from_digest(Digest::from_bytes([1; 32]));
        for cargo in [CLIPPY_ARGS.as_slice(), TEST_ARGS.as_slice()] {
            let run = request(tree, &bound(), cargo, &[]).expect("the fixed request builds");

            for step in run.steps.as_slice() {
                for (key, _) in ENV {
                    let value =
                        step.env.iter().find(|var| var.key() == key).expect("the step sets the variable").value();
                    let in_scratch = run.scratch.as_slice().iter().any(|scratch| {
                        let root = format!("/{}/{}", Mounts::WORK, scratch.as_str());
                        value == root || value.starts_with(&format!("{root}/"))
                    });
                    assert!(in_scratch, "{key}={value} lies outside every /work/<scratch>");
                }
            }
        }
    }

    #[test]
    fn the_cargo_config_is_mounted_above_work() {
        // Catches a dropped config mount, or one mounted where cargo's ancestor walk from /work never reaches it.
        let tree = Ref::from_digest(Digest::from_bytes([1; 32]));
        for cargo in [CLIPPY_ARGS.as_slice(), TEST_ARGS.as_slice()] {
            let run = request(tree, &bound(), cargo, &[]).expect("the fixed request builds");

            let mounted = run.mounts.as_slice().iter().find(|mount| mount.tree == bound().cargo_config());
            assert_eq!(mounted.map(|mount| mount.at.as_str()), Some(".cargo"));
        }
    }

    #[test]
    fn the_extra_variables_reach_the_cargo_step_only() {
        // Catches a test env leaking into the fmt step, and through it into clippy's run key and warm layer.
        let tree = Ref::from_digest(Digest::from_bytes([1; 32]));
        let extra = [EnvVar::new("AETHER_ALLOW_WASM_SKIP", "1").expect("test variable")];
        let plain = request(tree, &bound(), &TEST_ARGS, &[]).expect("the plain request builds");
        let varied = request(tree, &bound(), &TEST_ARGS, &extra).expect("the varied request builds");

        let [plain_fmt, plain_cargo] = plain.steps.as_slice() else {
            panic!("two steps")
        };
        let [fmt, cargo] = varied.steps.as_slice() else {
            panic!("two steps")
        };
        assert_eq!(fmt, plain_fmt);
        assert_eq!(cargo.env[..ENV.len()], plain_cargo.env[..]);
        assert_eq!(cargo.env[ENV.len()..], extra);
    }
}
