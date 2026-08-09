//! A stand-in for `cargo xtask transform`, for driving the lane boundary
//! without compiling a workspace or forking a model (#4727).
//!
//! Everything else about a dispatch stays real when this is mounted: the scratch
//! worktree `git worktree add` materializes, the environment scrub, the child
//! process, its exit status, the `evidence.json` it leaves on disk, the
//! candidate the coordinator captures out of the worktree. Only the program at
//! the end of the argv changes, through [`LaneProgram`]. That is the point — the
//! bugs this tier exists to catch all live in those steps, and a test double
//! mounted at [`TransformRunner`] skips every one of them.
//!
//! The behaviour of a run is chosen by a [`script`] the harness writes beside the
//! run directories, not by the nonce: the coordinator mints nonces, so a test
//! cannot name one in advance, and the lane environment scrub deliberately
//! strips every `AETHER_*` variable before the child starts. The script's
//! location is derived from `--out`, which is the one channel that survives both.
//!
//! [`LaneProgram`]: super::LaneProgram
//! [`TransformRunner`]: super::TransformRunner

use std::path::Path;
use std::{env, error, fmt, io, thread};

pub mod argv;
pub mod evidence;
pub mod script;

pub use argv::{ArgvError, LaneArgs};
pub use evidence::CANDIDATE_FILE;
pub use script::{LaneMode, LaneRun, LaneScript, LaneStep, read_ledger};

/// Why a mock run could not do its job. Distinct from a lane *failing*, which is
/// an outcome the script asked for and the evidence records.
#[derive(Debug)]
pub enum MockLaneError {
    /// The dispatch's argv was not shaped like a lane's.
    Argv(ArgvError),
    /// The script could not be read, or the evidence could not be written.
    Io(io::Error),
}

impl fmt::Display for MockLaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argv(error) => write!(f, "mock lane argv: {error}"),
            Self::Io(error) => write!(f, "mock lane io: {error}"),
        }
    }
}

impl error::Error for MockLaneError {}

impl From<ArgvError> for MockLaneError {
    fn from(error: ArgvError) -> Self {
        Self::Argv(error)
    }
}

impl From<io::Error> for MockLaneError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Where the script and ledger live for a run whose evidence goes to `out`.
///
/// The backend lays a run out as `<base>/<nonce>` (worktree) beside
/// `<base>/<nonce>-evidence` (`--out`), so the run directories' shared parent is
/// the one place both the harness and every child can name without agreeing on
/// anything else. A run whose `--out` has no parent falls back to `--out`
/// itself, which is wrong but recoverable — the script simply will not be found
/// and the run takes the default mode.
fn script_dir(out: &Path) -> &Path {
    out.parent().unwrap_or(out)
}

/// Run one mock lane: read the script, record the run, act, and report the exit
/// code the process should carry.
///
/// `worktree` is the directory the candidate (if any) is written into — the
/// scratch worktree the coordinator spawned the child in, which is the child's
/// own working directory in production.
///
/// # Errors
/// The argv was mis-shaped, or the script/evidence could not be read/written.
pub fn run<I: IntoIterator<Item = String>>(args: I, worktree: &Path) -> Result<i32, MockLaneError> {
    let args = argv::parse(args)?;
    let script_dir = script_dir(&args.out);

    // An absent or undecodable script is an all-passing one rather than a
    // failure: a scenario that scripts nothing wants the green path, and a
    // harness bug that loses the file should surface as a scenario assertion
    // rather than as every lane refusing to run.
    let script = LaneScript::read_from(script_dir).unwrap_or_default();
    let mode = script.mode_for(&args.command, script::occurrence_of(script_dir, &args.command)?);

    // Recorded before the run acts, so a mode that never exits still leaves
    // proof it was dispatched — which is exactly what the "every dispatched
    // order terminates" invariant needs to see.
    script::append_run(
        script_dir,
        &LaneRun {
            command: args.command.clone(),
            nonce: args.nonce.clone(),
            mode,
            subject: args.subject.clone(),
            diff_base: args.diff_base.clone(),
            task: args.task.clone(),
        },
    )?;

    let outcome = evidence::outcome(&args.command, &args.nonce, mode);
    evidence::apply(&outcome, worktree, &args.out)?;

    if mode == LaneMode::NeverExits {
        // Park rather than spin: the harness's own budget ends this run, and the
        // coordinator's staleness sweep is what the scenario is watching.
        loop {
            thread::park();
        }
    }

    Ok(outcome.exit_code)
}

/// [`run`] over the real process's arguments and working directory — the
/// binary's whole body.
///
/// # Errors
/// As [`run`], plus a working directory the process cannot read.
pub fn run_process() -> Result<i32, MockLaneError> {
    run(env::args().skip(1), &env::current_dir()?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a fixture that cannot set up its files reports it by panicking")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use aether_bloomery::{CONSTRUCT_IMPLEMENT_COMMAND, VERIFY_CHECK_COMMAND};

    use super::evidence::CANDIDATE_FILE;
    use super::script::{LaneMode, LaneScript, read_ledger};
    use super::{run, script_dir};

    // One dispatch's argv, laid out the way the backend lays a run out: the
    // worktree and the evidence dir as siblings under a shared base.
    fn dispatch(base: &Path, command: &str, nonce: &str) -> (Vec<String>, PathBuf) {
        let worktree = base.join(nonce);
        let out = base.join(format!("{nonce}-evidence"));
        fs::create_dir_all(&worktree).unwrap();
        (
            vec![
                command.to_owned(),
                "--out".to_owned(),
                out.to_string_lossy().into_owned(),
                "--nonce".to_owned(),
                nonce.to_owned(),
            ],
            worktree,
        )
    }

    #[test]
    fn successive_dispatches_of_one_command_walk_the_script_across_processes() {
        // Tripwire: the whole reason the script is consumed through an on-disk
        // ledger rather than in memory. Every run is a fresh process, so a
        // scenario like "fails verification twice, then lands" is three
        // independent invocations that must not all read step one.
        let base = tempfile::tempdir().unwrap();
        LaneScript::all_passing()
            .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
            .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
            .write_to(base.path())
            .unwrap();

        let modes: Vec<LaneMode> = ["n-1", "n-2", "n-3"]
            .iter()
            .map(|nonce| {
                let (args, worktree) = dispatch(base.path(), VERIFY_CHECK_COMMAND, nonce);
                run(args, &worktree).unwrap();
                read_ledger(base.path()).unwrap().last().unwrap().mode
            })
            .collect();

        assert_eq!(modes, [LaneMode::Fail, LaneMode::Fail, LaneMode::Pass]);
    }

    #[test]
    fn a_passing_construct_run_leaves_a_candidate_the_coordinator_can_capture() {
        // Tripwire: the coordinator captures a candidate with `git status
        // --porcelain` over the scratch worktree, so a construct lane that
        // wrote its file anywhere but there would claim a candidate the capture
        // could never find — and the run would downgrade to a failure whose
        // cause is invisible.
        let base = tempfile::tempdir().unwrap();
        let (args, worktree) = dispatch(base.path(), CONSTRUCT_IMPLEMENT_COMMAND, "n-1");

        assert_eq!(run(args, &worktree).unwrap(), 0);

        assert!(worktree.join(CANDIDATE_FILE).exists(), "the candidate lands in the worktree, not the evidence dir");
    }

    #[test]
    fn a_run_records_itself_before_it_acts() {
        // Tripwire: a lane that never exits must still be visible as dispatched,
        // or the "every dispatched order terminates" invariant has nothing to
        // read and a hung lane looks like one that was never sent.
        let base = tempfile::tempdir().unwrap();
        LaneScript::all_passing().then(VERIFY_CHECK_COMMAND, LaneMode::ExitsNonZero).write_to(base.path()).unwrap();
        let (args, worktree) = dispatch(base.path(), VERIFY_CHECK_COMMAND, "n-1");

        assert_eq!(run(args, &worktree).unwrap(), 2, "the mode's exit status reaches the caller");

        let ledger = read_ledger(base.path()).unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].nonce, "n-1");
    }

    #[test]
    fn the_script_is_found_beside_the_run_directories() {
        // Tripwire: the derivation is the only channel that survives both the
        // coordinator-minted nonce and the `AETHER_*` environment scrub. If it
        // drifted from the backend's `<base>/<nonce>-evidence` layout, every
        // scenario would silently run the default mode and the scripts would
        // become decoration.
        assert_eq!(script_dir(Path::new("/runs/n-1-evidence")), Path::new("/runs"));
    }
}
