//! Per-test triage of a failing `verify.test` run.
//!
//! The rule this replaces judged a whole member at once: one identical recheck
//! of everything, and a green second run excused every failure in the first.
//! That is too coarse in both directions. It excuses a real defect because some
//! *other* test in the same run stopped flaking, and it charges a candidate for
//! a test that was already red on the base it was cut from.
//!
//! So every failing test is triaged on its own, and only the last case is work:
//!
//! 1. **Replay it with the same input.** A property test replays the
//!    counterexample its first run shrank to and persisted; a plain test is an
//!    identical rerun of that one test. A replay that no longer names the test
//!    is a flake — recorded as such, never handed to a repair lap. A *different
//!    dice roll* is not proof of flakiness, which is why step 1 replays rather
//!    than re-samples.
//! 2. **Run it against the base.** Still red on the candidate, so ask whether
//!    the candidate is why: the same one test runs at the work order's diff
//!    base, in its own checkout. Red there too and it is pre-existing —
//!    recorded, and not this candidate's to fix.
//! 3. **Red only on the candidate.** The one case that becomes a finding.
//!
//! Both excusals are recorded rather than dropped: an excuse nobody can read is
//! indistinguishable from a gate that silently stopped checking, and the two
//! ledgers are what make a mis-triage visible.

use std::collections::BTreeSet;

use anyhow::Result;
use serde::Serialize;

use super::nextest::ClassifiedRun;

/// One test the triage declined to charge the candidate for.
///
/// The `replayed` half is what makes the record checkable: a flake names what
/// was re-run against the same input, and an inherited failure names the commit
/// it was still red at.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Excused {
    /// The nextest `binary-id test_name` pair.
    pub test: String,
    /// What the replay ran against — the persisted counterexample for a
    /// property test, the identical invocation for a plain one, or the base
    /// commit for an inherited failure.
    pub replayed: String,
    /// Wall-clock of the replay that produced this excusal, when that replay
    /// was a one-test spawn. Absent for a wholesale member re-run that never
    /// opened a per-test replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_millis: Option<u64>,
}

/// What a per-test triage concluded about one failing run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Triage {
    /// Tests that stopped failing when replayed against the same input.
    pub flakes: Vec<Excused>,
    /// Tests that were already red at the work order's base.
    pub inherited: Vec<Excused>,
    /// Tests red only on the candidate — the findings a repair lap is handed.
    pub findings: BTreeSet<String>,
}

impl Triage {
    /// The receipt for what was excused and why — the observation channel, not
    /// the findings channel: a repair lap handed these would chase a host or a
    /// defect it did not write.
    pub(super) fn observation(&self) -> Option<String> {
        if self.flakes.is_empty() && self.inherited.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        if !self.flakes.is_empty() {
            lines.push(format!(
                "{} failing {} did not repeat when replayed against the same input, so {} recorded as \
                 {} rather than handed to a repair lap:",
                self.flakes.len(),
                tests_word(self.flakes.len()),
                if self.flakes.len() == 1 {
                    "it is"
                } else {
                    "they are"
                },
                if self.flakes.len() == 1 {
                    "a flake"
                } else {
                    "flakes"
                },
            ));
            lines.extend(self.flakes.iter().map(|excused| excused.line("replayed")));
        }
        if !self.inherited.is_empty() {
            lines.push(format!(
                "{} failing {} already red at the work order's base, so {} pre-existing rather than this \
                 candidate's to fix:",
                self.inherited.len(),
                tests_word(self.inherited.len()),
                if self.inherited.len() == 1 {
                    "it was"
                } else {
                    "they were"
                },
            ));
            lines.extend(self.inherited.iter().map(|excused| excused.line("red at")));
        }
        Some(lines.join("\n"))
    }
}

impl Excused {
    /// One ledger line: the test, what it was replayed against, and the replay's
    /// wall-clock when the spawn measured one.
    fn line(&self, relation: &str) -> String {
        self.duration_millis.map_or_else(
            || format!("  {} ({relation} {})", self.test, self.replayed),
            |millis| format!("  {} ({relation} {}; {millis} millis)", self.test, self.replayed),
        )
    }
}

fn tests_word(count: usize) -> &'static str {
    if count == 1 {
        "test"
    } else {
        "tests"
    }
}

/// What one replay said about the test it was asked about.
pub(super) enum ReplayVerdict {
    /// The replay ran and did not name this test among its failures.
    Cleared,
    /// The replay named this test as failing again.
    Repeated,
    /// The replay could not compute a verdict at all — a build that would not
    /// run, a checkout that could not be made. Never an excusal: a triage step
    /// that did not happen must fail towards the finding.
    Unreached,
}

/// Triage every candidate failure `classified` named.
///
/// `replay` runs one test against the same input; `at_base` runs one test at
/// the work order's diff base, and is `None` when the run has no base to ask
/// (an aggregate verify, or a hand-run). With no base, step 2 is skipped and a
/// test that repeats goes straight to findings — which is the fail-towards-work
/// direction.
pub(super) fn triage(
    classified: &ClassifiedRun,
    base: Option<&str>,
    mut replay: impl FnMut(&str, Option<&str>) -> Result<(ReplayVerdict, String, u64)>,
) -> Result<Triage> {
    let mut triage = Triage::default();
    for test in classified.candidate_tests() {
        // Step 1. Only a replay that ran and cleared the test excuses it: a
        // replay that could not compute a verdict at all proves nothing, and
        // reading it as a pass would excuse a defect on the strength of a build
        // that never happened.
        let (verdict, replayed, duration_millis) = replay(&test, None)?;
        if matches!(verdict, ReplayVerdict::Cleared) {
            triage.flakes.push(Excused { test, replayed, duration_millis: Some(duration_millis) });
            continue;
        }
        // Step 2, when there is a base to ask. Only a base run that named this
        // test failing again excuses it — a base that would not build is
        // `Unreached` and falls through to the finding.
        let Some(base) = base else {
            triage.findings.insert(test);
            continue;
        };
        let (base_verdict, at, duration_millis) = replay(&test, Some(base))?;
        if matches!(base_verdict, ReplayVerdict::Repeated) {
            triage.inherited.push(Excused { test, replayed: at, duration_millis: Some(duration_millis) });
        } else {
            triage.findings.insert(test);
        }
    }
    Ok(triage)
}
