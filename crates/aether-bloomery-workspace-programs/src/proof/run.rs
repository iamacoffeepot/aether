//! The run a proof asks the workspace for: `cargo fmt`, then a cargo step
//! (ADR-0237 decision 12).
//!
//! Clippy's arguments are fixed, so its run key (ADR-0237 decision 9) is the
//! same for every proof in one environment, and clippy proofs share one
//! allotment estimate and one warm build layer. The test step takes the
//! bound's test env and the model's optional scope, so its key varies with
//! both; a scoped run names the whole key in `layer` and sits on the
//! whole-workspace test layer as a guest (see `whole_key`).

use aether_bloomery_kinds::{Refusal, Tree};
use aether_bloomery_workspace::{
    EnvVar, Mount, Mounts, Network, RunRequest, Scratch, Step, Steps, ToolName, TreePath, run_key,
};
use aether_data::{Digest, Ref};

use super::{ProofBound, TestScope, refused};

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

/// The scratch path that holds temporary files, the home, and cargo's home.
const TMP_SCRATCH: &str = "tmp";

/// Cargo's home, under [`TMP_SCRATCH`]: the root is read-only.
const CARGO_HOME: &str = "/work/tmp/cargo-home";

/// Cargo's build output, at [`TARGET_SCRATCH`].
const CARGO_TARGET_DIR: &str = "/work/target";

/// Temporary files, at [`TMP_SCRATCH`].
const TMPDIR: &str = "/work/tmp";

/// The home directory, under [`TMP_SCRATCH`]: the root is read-only, and a
/// child a test forks resolves its data and config directories from it.
const HOME: &str = "/work/tmp/home";

/// The variables each run gives every step, over the environment's own.
const ENV: [(&str, &str); 4] =
    [("CARGO_HOME", CARGO_HOME), ("CARGO_TARGET_DIR", CARGO_TARGET_DIR), ("HOME", HOME), ("TMPDIR", TMPDIR)];

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

/// The clippy step's argv as owned strings: [`CLIPPY_ARGS`] in order.
#[must_use]
pub(super) fn clippy_args() -> Vec<String> {
    CLIPPY_ARGS.iter().map(|&arg| arg.to_owned()).collect()
}

/// The test step's argv for `scope`: the [`TEST_ARGS`] base in order, then
/// one `--test` plus name pair per target in order, then `--` plus the
/// filters in order when any exist. The cargo-level build graph is identical
/// by construction: `--workspace` keeps the package set, and the `--`
/// separator forwards filters to test binaries untouched.
#[must_use]
pub(super) fn test_args(scope: &TestScope) -> Vec<String> {
    let mut args = TEST_ARGS.iter().map(|&arg| arg.to_owned()).collect::<Vec<_>>();
    for target in scope.targets() {
        args.push("--test".to_owned());
        args.push(target.as_str().to_owned());
    }
    if !scope.filters().is_empty() {
        args.push("--".to_owned());
        for filter in scope.filters() {
            args.push(filter.as_str().to_owned());
        }
    }
    args
}

/// The fmt step's argv as owned strings: [`FMT_ARGS`] in order.
fn fmt_args() -> Vec<String> {
    FMT_ARGS.iter().map(|&arg| arg.to_owned()).collect()
}

/// The whole-test run key over `bound`'s environment and the whole steps
/// with the same test env: the base a scoped run builds over as a guest.
/// Whole runs leave `layer` as `None` and own this layer.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the value a request constructor refused.
pub(super) fn whole_key(bound: &ProofBound) -> Result<Digest, Refusal> {
    let whole = test_args(&TestScope::default());
    let steps = Steps::new(vec![step(&fmt_args(), &[])?, step(&whole, bound.test_env().as_slice())?])
        .map_err(|error| refused(format!("the whole test steps: {error}")))?;
    Ok(run_key(bound.environment().digest(), &steps))
}

/// The run that formats `tree` in `bound`'s environment with the network
/// off, then runs the cargo step `cargo` with `extra` appended to its
/// variables, building over `layer` as a guest when set. Only the cargo step
/// takes `extra`, so the fmt step, and with an empty `extra` the whole
/// request, is the same whatever the bound's test env is. Whole runs pass
/// `None` and own their layer; scoped test runs pass the whole key and sit
/// on the whole-workspace test layer instead of building cold.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the value a request constructor refused.
pub(super) fn request(
    tree: Ref<Tree>,
    bound: &ProofBound,
    cargo: &[String],
    extra: &[EnvVar],
    layer: Option<Digest>,
) -> Result<RunRequest, Refusal> {
    let vendor = Mount { at: path(VENDOR)?, tree: bound.vendor() };
    let cargo_config = Mount { at: path(CARGO_CONFIG)?, tree: bound.cargo_config() };

    Ok(RunRequest {
        tree,
        environment: bound.environment(),
        mounts: Mounts::new(vec![vendor, cargo_config])
            .map_err(|error| refused(format!("the proof mounts: {error}")))?,
        steps: Steps::new(vec![step(&fmt_args(), &[])?, step(cargo, extra)?])
            .map_err(|error| refused(format!("the proof steps: {error}")))?,
        scratch: Scratch::new(vec![path(TARGET_SCRATCH)?, path(TMP_SCRATCH)?])
            .map_err(|error| refused(format!("the scratch paths: {error}")))?,
        network: Network::Off,
        layer,
    })
}

/// One cargo step with `args`, every variable in [`ENV`], and `extra`
/// appended after them.
fn step(args: &[String], extra: &[EnvVar]) -> Result<Step, Refusal> {
    Ok(Step {
        tool: ToolName::new(TOOL).map_err(|error| refused(format!("tool {TOOL:?}: {error}")))?,
        args: args.to_vec(),
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
    use aether_bloomery_workspace::{EnvVar, Mounts, run_key};
    use aether_data::{Digest, Ref};

    use super::{CLIPPY_ARGS, ENV, TEST_ARGS, clippy_args, request, test_args, whole_key};
    use crate::proof::{ProofBound, ScopeEntries, ScopeEntry, TestEnv, TestScope};

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
        for cargo in [clippy_args(), test_args(&TestScope::default())] {
            let run = request(tree, &bound(), &cargo, &[], None).expect("the fixed request builds");

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
        for cargo in [clippy_args(), test_args(&TestScope::default())] {
            let run = request(tree, &bound(), &cargo, &[], None).expect("the fixed request builds");

            let mounted = run.mounts.as_slice().iter().find(|mount| mount.tree == bound().cargo_config());
            assert_eq!(mounted.map(|mount| mount.at.as_str()), Some(".cargo"));
        }
    }

    #[test]
    fn the_extra_variables_reach_the_cargo_step_only() {
        // Catches a test env leaking into the fmt step, and through it into clippy's run key and warm layer.
        let tree = Ref::from_digest(Digest::from_bytes([1; 32]));
        let extra = [EnvVar::new("AETHER_ALLOW_WASM_SKIP", "1").expect("test variable")];
        let whole = test_args(&TestScope::default());
        let plain = request(tree, &bound(), &whole, &[], None).expect("the plain request builds");
        let varied = request(tree, &bound(), &whole, &extra, None).expect("the varied request builds");

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

    fn entry(value: &str) -> ScopeEntry {
        ScopeEntry::new(value).expect("test entry")
    }

    fn scope(targets: &[&str], filters: &[&str]) -> TestScope {
        let entries =
            |values: &[&str]| ScopeEntries::new(values.iter().map(|&value| entry(value)).collect()).expect("entries");
        TestScope::new(entries(targets), entries(filters))
    }

    #[test]
    fn the_default_scope_keeps_todays_whole_argv() {
        // Tripwire: the whole-workspace invocation keeps byte-identical argv,
        // run key, and warm layer to today.
        let whole = test_args(&TestScope::default());
        let expected = TEST_ARGS.iter().map(|&arg| arg.to_owned()).collect::<Vec<_>>();
        assert_eq!(whole, expected);
        assert_eq!(clippy_args(), CLIPPY_ARGS.iter().map(|&arg| arg.to_owned()).collect::<Vec<_>>());
    }

    #[test]
    fn scoped_argv_orders_targets_then_a_separator_then_filters() {
        // Catches a dropped or reordered axis: each target becomes one
        // `--test` pair in order, then `--` plus the filters in order.
        let scoped = test_args(&scope(&["session", "demo"], &["gate", "proof"]));
        let expected = TEST_ARGS
            .iter()
            .map(|&arg| arg.to_owned())
            .chain(["--test", "session", "--test", "demo", "--", "gate", "proof"].iter().map(|&arg| arg.to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(scoped, expected);

        assert_eq!(
            test_args(&scope(&["session"], &[])),
            TEST_ARGS
                .iter()
                .map(|&arg| arg.to_owned())
                .chain(["--test", "session"].iter().map(|&arg| arg.to_owned()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            test_args(&scope(&[], &["gate"])),
            TEST_ARGS
                .iter()
                .map(|&arg| arg.to_owned())
                .chain(["--", "gate"].iter().map(|&arg| arg.to_owned()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_whole_key_equals_the_whole_runs_key() {
        // Catches an extra-env omission forking the base: the guest base is
        // the whole steps hashed with the same test env the whole run uses.
        let bound = bound();
        let base = whole_key(&bound).expect("the whole key builds");
        let whole = test_args(&TestScope::default());
        let run =
            request(Ref::from_digest(Digest::from_bytes([1; 32])), &bound, &whole, bound.test_env().as_slice(), None)
                .expect("the whole request builds");
        assert_eq!(base, run_key(run.environment.digest(), &run.steps));
        assert_eq!(run.layer, None);
    }
}
