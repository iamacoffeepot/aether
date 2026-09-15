//! Triage outcomes an earlier step already recorded (#5979).
//!
//! A contextual verify fails four tests and triages them; the coordinator then
//! re-verifies the implicated member alone on the *same head*, and the gate
//! replays the same four names against the same base — eleven of that step's
//! eighteen minutes spent re-deriving an answer the previous step wrote down.
//! The per-test observations `verify.test` already writes (`ObservingRunner`)
//! are that answer; nothing read them back.
//!
//! So each triage step is keyed by the triple that determines its outcome —
//! **the test, the input it runs against, and the commit it runs at** — and a
//! step whose triple an earlier step's observations already answered is cited
//! instead of re-run.
//!
//! The input half is deliberately coarse: [`candidate_input`] answers only for
//! a clean checkout, and names the commit it stands at. A dirty tree has no
//! cheap identity that is *honestly* an identity — two different edits to one
//! file look the same to any summary short of the patch itself — and a memo
//! keyed by something that is not the input is a gate excusing a failure on
//! evidence about a different tree. With no input there is no memo, and every
//! triage step runs exactly as it did before.
//!
//! Where the earlier steps are is the executor's layout: one evidence directory
//! per dispatch, siblings under the same base. This module reads the sibling
//! directories and takes what it finds; a layout it does not recognize yields
//! an empty memo rather than an error, because a memo that cannot be built is
//! only ever slower, never wrong.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
#[cfg(test)]
use std::sync::OnceLock;

use serde::Deserialize;

use super::observations::ObservedResult;
use super::triage::ReplayVerdict;
use crate::cargo::run_captured;

/// What one earlier invocation recorded, as the memo reads it back.
///
/// Owned and permissive on purpose: the writer's shape is free to grow fields
/// this reader does not know, and an older step's file is free to lack the ones
/// it does.
#[derive(Deserialize)]
struct RecordedRun {
    /// The candidate input the whole file was recorded against.
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    invocations: Vec<RecordedInvocation>,
}

#[derive(Deserialize)]
struct RecordedInvocation {
    /// The commit this invocation ran at — `None` is the candidate's own tree.
    #[serde(default)]
    at: Option<String>,
    /// The one test a same-input replay was about, or `None` for an invocation
    /// that ran a set (a base run, or the member run itself).
    #[serde(default)]
    test: Option<String>,
    /// `test:<name>` → what that test did.
    #[serde(default)]
    outcomes: BTreeMap<String, ObservedResult>,
}

/// The prefix `ObservingRunner` writes a per-test outcome key under.
const TEST_KEY: &str = "test:";

/// Triage outcomes earlier steps recorded for this gate against this input.
#[derive(Default)]
pub(super) struct Memo {
    /// `(test, at)` → what that triage step concluded.
    recorded: BTreeMap<(String, Option<String>), ReplayVerdict>,
}

#[cfg(test)]
impl Memo {
    /// The shared empty memo — nothing recalled, so every triage step runs.
    ///
    /// Test-only: a run resolves its own memo from the evidence beside it, and
    /// the empty one is what a policy test drives the triage with when the
    /// question it asks is not about reuse.
    pub(super) fn none() -> &'static Self {
        static EMPTY: OnceLock<Memo> = OnceLock::new();
        EMPTY.get_or_init(Self::default)
    }
}

impl Memo {
    /// Read every evidence directory beside `logs` for `gate` observations
    /// recorded against `input`.
    ///
    /// `logs` itself is skipped: the file this run is writing holds its own
    /// member run and the replays it is in the middle of making, and a triage
    /// that could cite its own output would answer a question with itself.
    pub(super) fn open(logs: &Path, gate: &str, input: Option<&str>) -> Self {
        let (Some(input), Some(beside)) = (input, logs.parent()) else {
            return Self::default();
        };
        let Ok(entries) = fs::read_dir(beside) else {
            return Self::default();
        };

        let mut memo = Self::default();
        for directory in entries.flatten().map(|entry| entry.path()).filter(|path| path != logs) {
            memo.absorb(&directory.join(format!("{gate}.observations.json")), input);
        }
        memo
    }

    /// Take everything one earlier step's observations file says, when it was
    /// recorded against the same input.
    fn absorb(&mut self, file: &Path, input: &str) {
        let Some(recorded) =
            fs::read_to_string(file).ok().and_then(|body| serde_json::from_str::<RecordedRun>(&body).ok())
        else {
            return;
        };
        if recorded.input.as_deref() != Some(input) {
            return;
        }

        for invocation in recorded.invocations {
            // An invocation at the candidate's own tree is only a same-input
            // replay when it names the one test it was about. The member run
            // itself records per-test outcomes too, and reading those as
            // replays would answer "does it repeat?" with the very run that
            // asked — two observations of one invocation, not two invocations.
            if invocation.at.is_none() && invocation.test.is_none() {
                continue;
            }
            for (key, result) in invocation.outcomes {
                let Some(test) = key.strip_prefix(TEST_KEY) else {
                    continue;
                };
                if invocation.at.is_none() && invocation.test.as_deref() != Some(test) {
                    continue;
                }
                if let Some(verdict) = verdict_of(result) {
                    self.recorded.insert((test.to_owned(), invocation.at.clone()), verdict);
                }
            }
        }
    }

    /// What an earlier step concluded about this test at this commit, or `None`
    /// when no step did.
    pub(super) fn recall(&self, test: &str, at: Option<&str>) -> Option<ReplayVerdict> {
        self.recorded.get(&(test.to_owned(), at.map(ToOwned::to_owned))).copied()
    }
}

/// The verdict a recorded outcome is worth, or `None` when it is worth none.
///
/// Only the two explicit statuses carry: `Unknown` is a run whose reports
/// disagreed and `Infrastructure` is a run that did not judge anything, and
/// citing either would excuse a failure on the strength of a step that reached
/// no verdict.
fn verdict_of(result: ObservedResult) -> Option<ReplayVerdict> {
    match result {
        ObservedResult::Passed => Some(ReplayVerdict::Cleared),
        ObservedResult::Failed => Some(ReplayVerdict::Repeated),
        ObservedResult::Unknown | ObservedResult::Infrastructure => None,
    }
}

/// The candidate input this run's triage outcomes are keyed by: the commit the
/// working tree stands at, and only while it stands there cleanly.
///
/// Both halves are the key. The commit is what makes an earlier step's outcome
/// about the same code; the cleanliness is what makes the commit a complete
/// description of it. Anything git reports as modified, staged or untracked
/// withdraws the memo for the whole run — the safe direction, and the one that
/// costs only time.
pub(super) fn candidate_input() -> Option<String> {
    let head = git(&["rev-parse", "HEAD"])?;
    git(&["status", "--porcelain", "--untracked-files=all"])?.is_empty().then_some(head)
}

/// Run one read-only git query, trimmed, or `None` for anything short of a
/// clean success — which withdraws the memo rather than guessing at the tree.
fn git(args: &[&str]) -> Option<String> {
    let mut query = Command::new("git");
    query.args(args);

    let output = run_captured(query).ok()?;
    output.status.success().then(|| String::from_utf8(output.stdout).ok()).flatten().map(|text| text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::{env, fs, process};

    use super::{Memo, ReplayVerdict};

    /// One observations file as `ObservingRunner` writes it.
    fn observations(input: &str, invocations: &str) -> String {
        format!(
            "{{\"protocol\": 1, \"nonce\": \"n-1\", \"gate\": \"verify.test\", \"input\": \"{input}\", \
             \"invocations\": [{invocations}]}}"
        )
    }

    fn replay(test: &str, at: &str, result: &str) -> String {
        format!(
            "{{\"invocation\": \"dig-1\", \"at\": {at}, \"test\": \"{test}\", \
             \"outcomes\": {{\"test:{test}\": \"{result}\"}}}}"
        )
    }

    /// An evidence directory beside `beside`, holding `body` as its
    /// `verify.test` observations.
    fn step(root: &Path, name: &str, body: &str) -> PathBuf {
        let directory = root.join(name);
        fs::create_dir_all(&directory).expect("create the evidence directory");
        fs::write(directory.join("verify.test.observations.json"), body).expect("write the observations");
        directory
    }

    fn scratch(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!("aether-verify-memo-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create the scratch root");
        root
    }

    #[test]
    fn an_earlier_steps_outcome_for_the_same_triple_is_recalled() {
        // Acceptance for #5979's memo: same test, same input, same base — the
        // triple that determines the outcome. The measured case is exactly
        // this: a contextual verify triages four names, and the member-alone
        // re-verify on the same head asks the same base the same question.
        let root = scratch("recall");
        step(
            &root,
            "n-1-evidence",
            &observations(
                "headsha",
                &[replay("pkg::suite red", "\"basesha\"", "failed"), replay("pkg::suite flaky", "null", "passed")]
                    .join(","),
            ),
        );
        let ours = step(&root, "n-2-evidence", "");

        let memo = Memo::open(&ours, "verify.test", Some("headsha"));

        assert_eq!(memo.recall("pkg::suite red", Some("basesha")), Some(ReplayVerdict::Repeated));
        assert_eq!(memo.recall("pkg::suite flaky", None), Some(ReplayVerdict::Cleared));
        assert_eq!(memo.recall("pkg::suite red", None), None, "a base outcome says nothing about the candidate tree");
        assert_eq!(memo.recall("pkg::suite red", Some("otherbase")), None, "nor about a different base");
    }

    #[test]
    fn an_outcome_recorded_against_a_different_input_is_not_recalled() {
        // Tripwire for the half of the key that keeps the memo honest. The
        // outcome of a same-input replay is a statement about one tree; citing
        // it for another would excuse a failure the candidate did write, on
        // evidence about the code before it wrote it.
        let root = scratch("input");
        step(&root, "n-1-evidence", &observations("otherhead", &replay("pkg::suite red", "\"basesha\"", "failed")));
        let ours = step(&root, "n-2-evidence", "");

        assert_eq!(Memo::open(&ours, "verify.test", Some("headsha")).recall("pkg::suite red", Some("basesha")), None);
        assert_eq!(
            Memo::open(&ours, "verify.test", None).recall("pkg::suite red", Some("basesha")),
            None,
            "a run with no input of its own recalls nothing at all",
        );
    }

    #[test]
    fn the_member_runs_own_per_test_outcomes_are_not_a_replay() {
        // Tripwire for the one shape that would change a verdict rather than
        // save time. The member run records every test it ran, at the
        // candidate's own tree, with no `test` of its own. Reading those as
        // same-input replays would answer "did it repeat?" with the run that
        // asked the question, and a test that failed once would never be
        // replayed at all.
        let root = scratch("run");
        step(
            &root,
            "n-1-evidence",
            &observations(
                "headsha",
                "{\"invocation\": \"dig-0\", \"at\": null, \"test\": null, \
                 \"outcomes\": {\"test:pkg::suite red\": \"failed\", \"gate:verify.test\": \"failed\"}}",
            ),
        );
        let ours = step(&root, "n-2-evidence", "");

        assert_eq!(Memo::open(&ours, "verify.test", Some("headsha")).recall("pkg::suite red", None), None);
    }

    #[test]
    fn a_step_never_cites_its_own_observations() {
        // Tripwire: the file this run is writing holds the failing member run
        // and the replays it is part-way through. A memo that read it would
        // cite this step's own first replay back to it.
        let root = scratch("self");
        let ours = step(&root, "n-2-evidence", &observations("headsha", &replay("pkg::suite red", "null", "passed")));

        assert_eq!(Memo::open(&ours, "verify.test", Some("headsha")).recall("pkg::suite red", None), None);
    }

    #[test]
    fn an_inconclusive_outcome_is_never_cited() {
        // Tripwire: `unknown` is a run whose own reports disagreed and
        // `infrastructure` is one that judged nothing. Either would excuse a
        // failing test on a step that reached no verdict.
        let root = scratch("inconclusive");
        step(
            &root,
            "n-1-evidence",
            &observations(
                "headsha",
                &[
                    replay("pkg::suite disagreed", "\"basesha\"", "unknown"),
                    replay("pkg::suite faulted", "\"basesha\"", "infrastructure"),
                ]
                .join(","),
            ),
        );
        let ours = step(&root, "n-2-evidence", "");

        let memo = Memo::open(&ours, "verify.test", Some("headsha"));

        assert_eq!(memo.recall("pkg::suite disagreed", Some("basesha")), None);
        assert_eq!(memo.recall("pkg::suite faulted", Some("basesha")), None);
    }
}
