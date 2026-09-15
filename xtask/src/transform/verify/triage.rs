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
//!    the candidate is why: the tests that repeated run at the work order's
//!    diff base, in its own checkout. Red there too and it is pre-existing —
//!    recorded, and not this candidate's to fix.
//! 3. **Red only on the candidate.** The one case that becomes a finding.
//!
//! Step 2 is asked **once per step, for the whole repeating set** (#5979). The
//! decision is per test, but the work is one checkout, one build of the closure
//! at the base, and one nextest invocation that reports each name — a set of
//! four used to be four cold builds of the same commit, which measured at eleven
//! of a step's eighteen minutes. That is why [`Ask`] has two shapes rather than
//! one `(test, at)` pair: the unit a triage *decides* on is one test, and the
//! unit it *runs* is a set.
//!
//! Both excusals are recorded rather than dropped: an excuse nobody can read is
//! indistinguishable from a gate that silently stopped checking, and the two
//! ledgers are what make a mis-triage visible.

use std::collections::{BTreeMap, BTreeSet};

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
    /// Wall-clock of the invocation that produced this excusal. A base run
    /// covers the whole repeating set at once, so its tests share one figure —
    /// the cost of the answer, not of one test's share of it. Absent for an
    /// outcome an earlier step recorded and this one cited, and for a wholesale
    /// member re-run that never opened a per-test replay.
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
    /// Which of this triage's steps were answered from an earlier step's
    /// recorded outcomes rather than run again.
    pub reuse: Reuse,
}

/// What the triage cited and what it ran.
///
/// A memo that silently skips work is indistinguishable from a gate that
/// stopped doing it, so the counts and the cited names ride in the evidence
/// beside the excusals they justify.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Reuse {
    /// One line per cited triage step: the test and what the recorded outcome
    /// was about.
    pub cited: Vec<String>,
    /// How many triage steps this run spawned a process for.
    pub ran: usize,
}

impl Reuse {
    /// Record a triage step answered from an earlier step's observations.
    pub(super) fn cite(&mut self, test: &str, at: Option<&str>) {
        self.cited.push(at.map_or_else(
            || format!("  {test} (replayed against the same input by an earlier step)"),
            |base| format!("  {test} (run at {base} by an earlier step)"),
        ));
    }

    /// The paragraph an operator reads to see the memo hit, or `None` when
    /// nothing was cited.
    fn observation(&self) -> Option<String> {
        if self.cited.is_empty() {
            return None;
        }
        let mut lines = vec![format!(
            "{} triage {} answered from the outcomes an earlier step already recorded for the same test, the same \
             input and the same base, so {} not re-run here ({} {} spawned):",
            self.cited.len(),
            if self.cited.len() == 1 {
                "step was"
            } else {
                "steps were"
            },
            if self.cited.len() == 1 {
                "it was"
            } else {
                "they were"
            },
            self.ran,
            if self.ran == 1 {
                "was"
            } else {
                "were"
            },
        )];
        lines.extend(self.cited.iter().cloned());
        Some(lines.join("\n"))
    }

    /// The clause the triaged member's own log carries, so the wall-clock a
    /// reader compares against is accounted for in the artifact too.
    pub(super) fn notice(&self) -> String {
        if self.cited.is_empty() {
            return String::new();
        }
        format!(
            " {} of those triage steps were cited from an earlier step's recorded outcomes rather than re-run, and \
             {} ran here.",
            self.cited.len(),
            self.ran,
        )
    }
}

impl Triage {
    /// The receipt for what was excused and why — the observation channel, not
    /// the findings channel: a repair lap handed these would chase a host or a
    /// defect it did not write.
    pub(super) fn observation(&self) -> Option<String> {
        if self.flakes.is_empty() && self.inherited.is_empty() && self.reuse.cited.is_empty() {
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
        lines.extend(self.reuse.observation());
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// What a triage step asks for.
///
/// The two shapes are the two questions, and they are asked at different
/// granularities on purpose: the same-input replay is about one test's own
/// recorded input, while the base question is about a commit and is answered
/// for every repeating test by one build of it.
#[derive(Clone, Copy)]
pub(super) enum Ask<'a> {
    /// Replay one test against the same input, on the candidate's own tree.
    SameInput(&'a str),
    /// Run every test in `tests` at `base`, in one checkout and one invocation.
    AtBase {
        /// The tests that repeated against the same input.
        tests: &'a [String],
        /// The work order's diff base.
        base: &'a str,
    },
}

/// What one asked-about test came back as.
pub(super) struct Outcome {
    /// The verdict for that test.
    pub verdict: ReplayVerdict,
    /// What the ledger records the test as having been run against.
    pub label: String,
    /// The wall-clock of the invocation that produced it, or `None` when the
    /// outcome was cited from an earlier step rather than run.
    pub duration_millis: Option<u64>,
}

/// One triage step's answer, by test. A test the answer does not name is
/// [`ReplayVerdict::Unreached`]: an absent verdict is not evidence, and reading
/// it as one would excuse a failure on a run that never judged it.
pub(super) type Answers = BTreeMap<String, Outcome>;

/// Triage every candidate failure `classified` named.
///
/// `ask` runs one triage step: a same-input replay of one test, or one run of
/// the whole repeating set at `base`. `base` is `None` when the run has no base
/// to ask (an aggregate verify, or a hand-run); step 2 is then skipped and a
/// test that repeats goes straight to findings, which is the fail-towards-work
/// direction.
pub(super) fn triage(
    classified: &ClassifiedRun,
    base: Option<&str>,
    mut ask: impl FnMut(Ask<'_>) -> Result<Answers>,
) -> Result<Triage> {
    let mut triage = Triage::default();
    let mut repeating: Vec<String> = Vec::new();
    for test in classified.candidate_tests() {
        // Step 1. Only a replay that ran and cleared the test excuses it: a
        // replay that could not compute a verdict at all proves nothing, and
        // reading it as a pass would excuse a defect on the strength of a build
        // that never happened.
        let answers = ask(Ask::SameInput(&test))?;
        match answers.get(&test) {
            Some(outcome) if outcome.verdict == ReplayVerdict::Cleared => triage.flakes.push(Excused {
                test,
                replayed: outcome.label.clone(),
                duration_millis: outcome.duration_millis,
            }),
            _ => repeating.push(test),
        }
    }

    // Step 2, when there is a base to ask. One build of it answers the whole
    // set; only a base run that named a test failing again excuses that test,
    // so a base that would not build leaves every name in the findings.
    let Some(base) = base.filter(|_| !repeating.is_empty()) else {
        triage.findings.extend(repeating);
        return Ok(triage);
    };
    let answers = ask(Ask::AtBase { tests: &repeating, base })?;
    for test in repeating {
        match answers.get(&test) {
            Some(outcome) if outcome.verdict == ReplayVerdict::Repeated => triage.inherited.push(Excused {
                test,
                replayed: outcome.label.clone(),
                duration_millis: outcome.duration_millis,
            }),
            _ => {
                triage.findings.insert(test);
            }
        }
    }
    Ok(triage)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::iter;

    use super::{Answers, Ask, Outcome, ReplayVerdict, triage};
    use crate::transform::verify::nextest;

    /// A failing nextest log naming each of `failures`.
    fn failing_run(failures: &[&str]) -> String {
        let mut body = String::new();
        for test in failures {
            let _ = writeln!(body, "        FAIL [   0.008s] (  1/900) {test}");
        }
        format!("{body}     Summary [  74.6s] 900 tests run: {} failed, 0 skipped\n", failures.len())
    }

    fn answer(test: &str, verdict: ReplayVerdict) -> Answers {
        Answers::from([(test.to_owned(), Outcome { verdict, label: "somewhere".to_owned(), duration_millis: None })])
    }

    #[test]
    fn the_whole_repeating_set_is_asked_of_the_base_in_one_step() {
        // Tripwire for #5979. The decision is per test and the work is not: a
        // base run is a checkout plus a cold build of the closure at that
        // commit, and asking it once per failing name priced a four-failure
        // member at four of them — eleven of the measured step's eighteen
        // minutes. An `Ask::AtBase` per test would pass every other assertion
        // in this file, so the count is the assertion.
        let classified = nextest::classify(&failing_run(&["pkg::suite red_one", "pkg::suite red_two"]), None)
            .expect("the log names two candidate failures");
        let mut asked: Vec<String> = Vec::new();

        let triaged = triage(&classified, Some("deadbeef"), |ask| match ask {
            Ask::SameInput(test) => {
                asked.push(format!("same-input {test}"));
                Ok(answer(test, ReplayVerdict::Repeated))
            }
            Ask::AtBase { tests, base } => {
                asked.push(format!("base {base} over {}", tests.len()));
                Ok(tests
                    .iter()
                    .map(|test| {
                        (
                            test.clone(),
                            Outcome {
                                verdict: ReplayVerdict::Repeated,
                                label: base.to_owned(),
                                duration_millis: Some(7),
                            },
                        )
                    })
                    .collect())
            }
        })
        .expect("the triage runs");

        assert_eq!(asked, ["same-input pkg::suite red_one", "same-input pkg::suite red_two", "base deadbeef over 2",]);
        assert_eq!(triaged.inherited.len(), 2, "both were already red at the base");
        assert!(triaged.findings.is_empty());
    }

    #[test]
    fn a_base_run_that_names_only_one_of_the_set_keeps_the_other_as_a_finding() {
        // Tripwire for the direction batching could break. One invocation now
        // answers several tests, so the per-test reading of its output is the
        // whole discrimination: a set-level "the base was red" would excuse
        // every test in the batch on the strength of one of them.
        let classified = nextest::classify(&failing_run(&["pkg::suite inherited", "pkg::suite mine"]), None)
            .expect("the log names two candidate failures");

        let triaged = triage(&classified, Some("deadbeef"), |ask| match ask {
            Ask::SameInput(test) => Ok(answer(test, ReplayVerdict::Repeated)),
            Ask::AtBase { base, .. } => Ok(answer("pkg::suite inherited", ReplayVerdict::Repeated)
                .into_iter()
                .chain(iter::once((
                    "pkg::suite mine".to_owned(),
                    Outcome { verdict: ReplayVerdict::Cleared, label: base.to_owned(), duration_millis: Some(3) },
                )))
                .collect()),
        })
        .expect("the triage runs");

        assert_eq!(triaged.inherited.len(), 1);
        assert_eq!(triaged.inherited[0].test, "pkg::suite inherited");
        assert_eq!(triaged.findings.iter().collect::<Vec<_>>(), ["pkg::suite mine"]);
    }

    #[test]
    fn a_base_run_that_never_judged_a_test_leaves_it_a_finding() {
        // Tripwire: an absent verdict is not evidence. A batched base run that
        // dies half-way names some of its set and not the rest, and reading the
        // silence as "not red at the base" would be the same false green a
        // build that never ran gives.
        let classified =
            nextest::classify(&failing_run(&["pkg::suite unjudged"]), None).expect("the log names one failure");

        let triaged = triage(&classified, Some("deadbeef"), |ask| match ask {
            Ask::SameInput(test) => Ok(answer(test, ReplayVerdict::Repeated)),
            Ask::AtBase { .. } => Ok(Answers::new()),
        })
        .expect("the triage runs");

        assert!(triaged.inherited.is_empty(), "silence excuses nothing");
        assert_eq!(triaged.findings.len(), 1);
    }

    #[test]
    fn a_cleared_same_input_replay_never_reaches_the_base() {
        // Tripwire for the step order: the base build is the expensive half,
        // and a flake that already cleared has nothing to ask it.
        let classified =
            nextest::classify(&failing_run(&["pkg::suite flaky"]), None).expect("the log names one failure");
        let mut base_runs = 0usize;

        let triaged = triage(&classified, Some("deadbeef"), |ask| match ask {
            Ask::SameInput(test) => Ok(answer(test, ReplayVerdict::Cleared)),
            Ask::AtBase { .. } => {
                base_runs += 1;
                Ok(Answers::new())
            }
        })
        .expect("the triage runs");

        assert_eq!(base_runs, 0, "a cleared test asks the base nothing");
        assert_eq!(triaged.flakes.len(), 1);
        assert!(triaged.findings.is_empty());
    }
}
