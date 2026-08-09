//! What a mock lane run does, and how one run's behaviour is chosen.
//!
//! A scenario needs the same lane program to behave differently on successive
//! dispatches — a member that fails verification twice and then passes is three
//! runs of one binary. The nonce cannot select that: the coordinator mints it,
//! so a test cannot name it in advance. The selection is therefore a **script**
//! the harness writes beside the run directories: an ordered list of steps, each
//! naming a lane command and the mode that command's *next* run takes.
//!
//! Consumption has to survive process exit, because every run is a fresh
//! process. So each run appends a line to a ledger next to the script and counts
//! the prior lines for its own command to find its index — the (n+1)-th run of
//! `verify.check` takes the (n+1)-th `verify.check` step. Past the last matching
//! step the script's `default` mode repeats, so an unbounded retry loop keeps
//! failing rather than falling off the end.
//!
//! The ledger is also the harness's record of what actually ran. It is the only
//! place a test can see that a review lane was handed `--diff-base`, or that a
//! wedge ceiling was reached after exactly three dispatches and not four.

use std::fmt;
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

/// The file name the harness writes its script under, and the mock reads it
/// from — resolved against the run's `--out` parent so no environment variable
/// has to survive the lane environment scrub (#4714).
pub const SCRIPT_FILE: &str = "mock-lane-script.json";

/// The file name each run appends its record to, beside the script.
pub const LEDGER_FILE: &str = "mock-lane-ledger.jsonl";

/// What one mock lane run does — its evidence, its exit status, and what it
/// leaves in the scratch worktree.
///
/// Every variant reproduces something a real lane has done in production; the
/// docs name which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaneMode {
    /// The lane concluded and its work stands: a verify/review `status: pass`,
    /// or a construct run that writes a candidate file and stamps
    /// `produced_candidate: true`.
    Pass,
    /// The lane concluded and its work does not stand: `status: fail` with
    /// findings, or a construct run that reports no candidate. Exits non-zero
    /// the way the real verify lane does.
    Fail,
    /// The review critic's third verdict: a ground step that could not execute
    /// at all, which is a host fault rather than a finding against the
    /// candidate and must not drive a repair lap.
    Environment,
    /// A construct run that concludes and stamps `produced_candidate: true`
    /// while leaving the worktree clean — the empty candidate that reaches
    /// review with nothing in it.
    ConcludesWithoutWriting,
    /// Exit zero having written no `evidence.json` at all.
    NoEvidence,
    /// Write a zero-byte `evidence.json` — the full-disk lane, whose wedge
    /// digest was the sha256 of nothing.
    EmptyEvidence,
    /// Write bytes that do not decode as JSON.
    MalformedEvidence,
    /// Exit non-zero having written nothing — an environment failure, as
    /// distinct from a candidate that failed its gate.
    ExitsNonZero,
    /// Never exit. The harness's own budget is what ends the run.
    NeverExits,
}

impl fmt::Display for LaneMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Through serde so one spelling serves the script, the ledger, and any
        // message a scenario prints — a second hand-written table here would be
        // free to drift from the one the script parses.
        match serde_json::to_value(self).ok().and_then(|value| value.as_str().map(str::to_owned)) {
            Some(name) => f.write_str(&name),
            None => write!(f, "{self:?}"),
        }
    }
}

/// One scripted run: the next run of `command` takes `mode`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneStep {
    /// The transform command id this step applies to (`verify.check`,
    /// `construct.implement`, `review.critic`).
    pub command: String,
    /// What that run does.
    pub mode: LaneMode,
}

impl LaneStep {
    /// A step naming `mode` for the next run of `command`.
    #[must_use]
    pub fn new(command: impl Into<String>, mode: LaneMode) -> Self {
        Self { command: command.into(), mode }
    }
}

/// The whole script: ordered steps, plus the mode every run past its command's
/// last step takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneScript {
    /// Consumed in order, per command.
    pub steps: Vec<LaneStep>,
    /// What a run takes once its command's steps are exhausted. Repeating
    /// rather than erroring is deliberate: a wedge scenario dispatches an
    /// unbounded number of times, and the script should not have to predict the
    /// ceiling it is trying to observe.
    pub default: LaneMode,
}

impl Default for LaneScript {
    fn default() -> Self {
        Self { steps: Vec::new(), default: LaneMode::Pass }
    }
}

impl LaneScript {
    /// A script whose every run passes.
    #[must_use]
    pub fn all_passing() -> Self {
        Self::default()
    }

    /// A script whose runs past the listed steps take `default`.
    #[must_use]
    pub fn with_default(mut self, default: LaneMode) -> Self {
        self.default = default;
        self
    }

    /// Append a step for the next run of `command`.
    #[must_use]
    pub fn then(mut self, command: impl Into<String>, mode: LaneMode) -> Self {
        self.steps.push(LaneStep::new(command, mode));
        self
    }

    /// The mode the `occurrence`-th (zero-based) run of `command` takes.
    #[must_use]
    pub fn mode_for(&self, command: &str, occurrence: usize) -> LaneMode {
        self.steps.iter().filter(|step| step.command == command).nth(occurrence).map_or(self.default, |step| step.mode)
    }

    /// Write the script to `dir` under [`SCRIPT_FILE`].
    ///
    /// # Errors
    /// The directory could not be created or the file could not be written.
    pub fn write_to(&self, dir: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let path = dir.join(SCRIPT_FILE);
        fs::write(&path, serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        Ok(path)
    }

    /// Read the script `dir` holds.
    ///
    /// # Errors
    /// The file is absent, unreadable, or does not decode.
    pub fn read_from(dir: &Path) -> io::Result<Self> {
        serde_json::from_slice(&fs::read(dir.join(SCRIPT_FILE))?).map_err(io::Error::other)
    }
}

/// One run's record, appended to the ledger as it starts.
///
/// Written *before* the run acts, so a mode that never exits still leaves proof
/// it was dispatched — the record has to survive the behaviour it describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneRun {
    /// The transform command id.
    pub command: String,
    /// The idempotency nonce the coordinator minted.
    pub nonce: String,
    /// What this run did.
    pub mode: LaneMode,
    /// The `--subject` the coordinator resolved, when the lane was handed one.
    pub subject: Option<String>,
    /// The `--diff-base` the coordinator resolved. Absent is the working-tree
    /// contract a member lane runs under; present is the committed range an
    /// aggregate review judges.
    pub diff_base: Option<String>,
    /// The advisory work order (`--task`), when one was threaded.
    pub task: Option<String>,
}

/// Append `run` to the ledger in `dir`.
///
/// # Errors
/// The ledger could not be opened or appended to.
pub fn append_run(dir: &Path, run: &LaneRun) -> io::Result<()> {
    use std::io::Write as _;

    let mut line = serde_json::to_vec(run).map_err(io::Error::other)?;
    line.push(b'\n');
    // Append-only and opened per run: several lanes can be in flight at once,
    // and a single short write under `O_APPEND` is atomic enough for one JSON
    // line, where a read-modify-write would interleave and lose records.
    fs::OpenOptions::new().create(true).append(true).open(dir.join(LEDGER_FILE))?.write_all(&line)
}

/// Every run the ledger in `dir` has recorded, in dispatch order. An absent
/// ledger is an empty history rather than an error — nothing has run yet.
///
/// # Errors
/// The ledger exists but could not be read.
pub fn read_ledger(dir: &Path) -> io::Result<Vec<LaneRun>> {
    let raw = match fs::read_to_string(dir.join(LEDGER_FILE)) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    // A concurrently-appending writer can leave a torn final line; skipping
    // what does not decode keeps a reader from failing on a record that is
    // still being written rather than on one that is wrong.
    Ok(raw.lines().filter_map(|line| serde_json::from_str(line).ok()).collect())
}

/// How many runs of `command` the ledger in `dir` already holds — this run's
/// zero-based occurrence index.
///
/// # Errors
/// The ledger exists but could not be read.
pub fn occurrence_of(dir: &Path, command: &str) -> io::Result<usize> {
    Ok(read_ledger(dir)?.iter().filter(|run| run.command == command).count())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a fixture that cannot set up its files reports it by panicking")]
mod tests {
    use super::{LaneMode, LaneRun, LaneScript, append_run, occurrence_of, read_ledger};

    #[test]
    fn successive_runs_of_one_command_walk_its_steps_in_order() {
        let script = LaneScript::all_passing()
            .then("verify.check", LaneMode::Fail)
            .then("construct.implement", LaneMode::Pass)
            .then("verify.check", LaneMode::Pass);

        assert_eq!(script.mode_for("verify.check", 0), LaneMode::Fail);
        assert_eq!(script.mode_for("verify.check", 1), LaneMode::Pass);
        assert_eq!(
            script.mode_for("construct.implement", 0),
            LaneMode::Pass,
            "a step for one command must not be consumed by another's run",
        );
    }

    #[test]
    fn a_command_past_its_last_step_repeats_the_default() {
        // Tripwire: the wedge scenarios dispatch until a ceiling the script is
        // trying to *observe*, so it cannot enumerate the runs in advance. If
        // an exhausted script fell back to passing, every wedge scenario would
        // silently become a green-path scenario.
        let script = LaneScript::all_passing().with_default(LaneMode::Fail).then("verify.check", LaneMode::Pass);

        assert_eq!(script.mode_for("verify.check", 0), LaneMode::Pass);
        assert_eq!(script.mode_for("verify.check", 1), LaneMode::Fail);
        assert_eq!(script.mode_for("verify.check", 9), LaneMode::Fail);
    }

    #[test]
    fn the_ledger_counts_each_commands_runs_separately() {
        let dir = tempfile::tempdir().unwrap();
        let run = |command: &str, nonce: &str| LaneRun {
            command: command.to_owned(),
            nonce: nonce.to_owned(),
            mode: LaneMode::Pass,
            subject: None,
            diff_base: None,
            task: None,
        };

        append_run(dir.path(), &run("verify.check", "n-1")).unwrap();
        append_run(dir.path(), &run("construct.implement", "n-2")).unwrap();
        append_run(dir.path(), &run("verify.check", "n-3")).unwrap();

        assert_eq!(occurrence_of(dir.path(), "verify.check").unwrap(), 2);
        assert_eq!(occurrence_of(dir.path(), "review.critic").unwrap(), 0);
        assert_eq!(read_ledger(dir.path()).unwrap().len(), 3, "every run is recorded, whatever its command");
    }

    #[test]
    fn an_unwritten_ledger_reads_as_no_runs_rather_than_a_failure() {
        // Tripwire: the first dispatch of every scenario reads the ledger
        // before anything has written one. Treating absence as an error would
        // fail every scenario's first lane instead of only a broken one.
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(occurrence_of(dir.path(), "verify.check").unwrap(), 0);
    }
}
