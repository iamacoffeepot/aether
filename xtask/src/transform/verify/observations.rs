//! Raw invocation observations for the host's contextual proof ledger.
//!
//! This file is additive evidence, not an authority to complete a logical
//! request. The host binds it to the issued node, contract, nonce and slot.

use std::collections::BTreeMap;
use std::path::Path;

use aether_bloomery::Digest;
use aether_bloomery::digest::{ContentAddressed, digest_of};
use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::nextest::{captured_output_header, status_line_test};
use super::{
    Captured, MemberOutcome, MemberRunner, Scope, VerifyInvocation, clippy_verdict, host_fault_in, member_outcome,
};
use crate::cargo::write_json_pretty;

/// Only an explicit test status line can create a per-test observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ObservedResult {
    Passed,
    Failed,
    Unknown,
    Infrastructure,
}

#[derive(Serialize)]
struct InvocationObservation {
    invocation: Digest,
    /// None is this order's candidate; Some is an explicitly pinned baseline.
    at: Option<String>,
    /// The one test a same-input replay was about, or `None` for an invocation
    /// that ran a set — the member run itself, or one base run over the whole
    /// repeating set.
    ///
    /// Serialized, not only hashed into the identity above, because a later
    /// step reads these records back as a triage memo (#5979) and the two kinds
    /// of candidate-tree invocation answer different questions: a replay's
    /// outcome is a second observation of the same input, while the member
    /// run's is the observation the replay exists to check.
    test: Option<String>,
    outcomes: BTreeMap<String, ObservedResult>,
}

#[derive(Serialize)]
struct Observations<'a> {
    protocol: u32,
    nonce: Option<&'a str>,
    gate: &'a str,
    /// The candidate input every invocation here ran against, or `None` when
    /// this run's tree had no single identity to name (see
    /// [`super::memo::candidate_input`]). A later step cites these outcomes
    /// only for the same input, so an absent one is a file nothing recalls.
    input: Option<&'a str>,
    invocations: &'a [InvocationObservation],
}

#[derive(Serialize)]
struct InvocationIdentity<'a> {
    nonce: Option<&'a str>,
    gate: &'a str,
    ordinal: usize,
    test: Option<&'a str>,
    at: Option<&'a str>,
}

impl ContentAddressed for InvocationIdentity<'_> {
    const DOMAIN: &'static str = "aether.bloomery.verify_invocation_observation.v1";
}

/// Wrap actual spawns rather than deriving a second report from a terminal
/// umbrella verdict. A replay is written only after the second process ran.
pub(super) struct ObservingRunner<'a> {
    inner: &'a mut dyn MemberRunner,
    gate: &'a str,
    nonce: Option<&'a str>,
    logs: &'a Path,
    input: Option<&'a str>,
    observations: Vec<InvocationObservation>,
}

impl<'a> ObservingRunner<'a> {
    pub(super) fn new(
        inner: &'a mut dyn MemberRunner,
        gate: &'a str,
        nonce: Option<&'a str>,
        logs: &'a Path,
        input: Option<&'a str>,
    ) -> Self {
        Self { inner, gate, nonce, logs, input, observations: Vec::new() }
    }

    fn record(
        &mut self,
        test: Option<&str>,
        at: Option<&str>,
        outcomes: BTreeMap<String, ObservedResult>,
    ) -> Result<()> {
        self.observations.push(InvocationObservation {
            invocation: digest_of(&InvocationIdentity {
                nonce: self.nonce,
                gate: self.gate,
                ordinal: self.observations.len(),
                test,
                at,
            }),
            at: at.map(ToOwned::to_owned),
            test: test.map(ToOwned::to_owned),
            outcomes,
        });
        write_json_pretty(
            &self.logs.join(format!("{}.observations.json", self.gate)),
            &Observations {
                protocol: 1,
                nonce: self.nonce,
                gate: self.gate,
                input: self.input,
                invocations: &self.observations,
            },
        )
    }
}

impl MemberRunner for ObservingRunner<'_> {
    fn run(&mut self, invocation: &VerifyInvocation, scope: &Scope, diff_base: Option<&str>) -> Result<Captured> {
        let captured = self.inner.run(invocation, scope, diff_base)?;
        let stdout = String::from_utf8_lossy(&captured.stdout);
        let stderr = String::from_utf8_lossy(&captured.stderr);
        let derived_pass = self.gate != "verify.clippy" || clippy_verdict(&stdout, scope);
        let verdict = if host_fault_in(&stdout).or_else(|| host_fault_in(&stderr)).is_some() {
            ObservedResult::Infrastructure
        } else {
            match member_outcome(invocation, derived_pass, captured.code) {
                MemberOutcome::Passed => ObservedResult::Passed,
                MemberOutcome::Failed => ObservedResult::Failed,
                MemberOutcome::Operational | MemberOutcome::Environment => ObservedResult::Infrastructure,
            }
        };
        let mut outcomes = if self.gate == "verify.test" {
            test_observations(&format!("{stdout}\n{stderr}"))
                .into_iter()
                .map(|(test, result)| (format!("test:{test}"), result))
                .collect()
        } else {
            BTreeMap::new()
        };
        outcomes.insert(format!("gate:{}", self.gate), verdict);
        self.record(None, None, outcomes)?;
        Ok(captured)
    }

    fn replay(&mut self, invocation: &VerifyInvocation, test: &str) -> Result<Captured> {
        let captured = self.inner.replay(invocation, test)?;
        let outcomes = replayed_outcomes(&captured, &[test.to_owned()]);
        self.record(Some(test), None, outcomes)?;
        Ok(captured)
    }

    fn replay_at_base(&mut self, invocation: &VerifyInvocation, tests: &[String], base: &str) -> Result<Captured> {
        let captured = self.inner.replay_at_base(invocation, tests, base)?;
        let outcomes = replayed_outcomes(&captured, tests);
        // No `test`: the invocation ran a set, and every name in it is recorded
        // in the outcomes with its own verdict.
        self.record(None, Some(base), outcomes)?;
        Ok(captured)
    }
}

/// What one replay invocation observed about each test it was asked to run.
///
/// A successful build or a different failing test proves nothing about a target
/// the log never named, so every asked-about test that the output does not
/// report on is written down as unjudged rather than left out. In particular
/// exit 101 is not a test verdict.
fn replayed_outcomes(captured: &Captured, tests: &[String]) -> BTreeMap<String, ObservedResult> {
    let output =
        format!("{}\n{}", String::from_utf8_lossy(&captured.stdout), String::from_utf8_lossy(&captured.stderr));
    let mut outcomes = test_observations(&output);
    let unjudged = if host_fault_in(&output).is_some() || captured.code.is_none() {
        ObservedResult::Infrastructure
    } else {
        ObservedResult::Unknown
    };
    for test in tests {
        outcomes.entry(test.clone()).or_insert(unjudged);
    }
    outcomes.into_iter().map(|(test, result)| (format!("test:{test}"), result)).collect()
}

/// Read nextest's per-test status lines into stable identities.
///
/// A run that never printed `Summary` supplies no facts. The closing summary
/// restates failures and is the only block that cannot contain a test's own
/// captured output, so it is the authority for a red run. Passing runs restate
/// nothing there; in-flight PASS lines before Summary — and before any captured
/// output banner — are then the baseline's per-test ledger. The in-flight
/// `(n/m)` progress counter is stripped: the same test observed by runs of
/// different sizes is one key.
fn test_observations(log: &str) -> BTreeMap<String, ObservedResult> {
    let Some(summary) = log
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("Summary ["))
        .map(|(index, _)| index)
        .last()
    else {
        return BTreeMap::new();
    };
    let restated = collect_status_lines(log.lines().skip(summary + 1));
    if !restated.is_empty() {
        return restated;
    }
    collect_status_lines(log.lines().take(summary).take_while(|line| captured_output_header(line).is_none()))
}

fn collect_status_lines<'a, I>(lines: I) -> BTreeMap<String, ObservedResult>
where
    I: Iterator<Item = &'a str>,
{
    let mut outcomes = BTreeMap::new();
    for line in lines {
        let Some((status, name)) = status_line_test(line) else {
            continue;
        };
        let verdict = match status {
            "PASS" => ObservedResult::Passed,
            "FAIL" | "TIMEOUT" | "ABORT" => ObservedResult::Failed,
            _ => continue,
        };
        outcomes
            .entry(name)
            .and_modify(|previous| {
                if *previous != verdict {
                    *previous = ObservedResult::Unknown;
                }
            })
            .or_insert(verdict);
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_build_and_omitted_test_are_unknown() {
        assert!(test_observations("error: could not compile crate\n").is_empty());
        assert!(test_observations("Summary [1s] 1 tests run: 1 passed\n").is_empty());
    }

    #[test]
    fn only_reported_final_statuses_become_observations() {
        let observed = test_observations(
            "\
--- STDOUT: fixture ---\n\
PASS [0s] forged::test\n\
Summary [1s] 2 tests run: 1 passed, 1 failed\n\
PASS [0.1s] package::suite observed_green\n\
FAIL [0.2s] package::suite observed_red\n",
        );
        assert_eq!(observed.len(), 2);
        assert_eq!(observed.get("package::suite observed_green"), Some(&ObservedResult::Passed));
        assert_eq!(observed.get("package::suite observed_red"), Some(&ObservedResult::Failed));
        assert!(!observed.contains_key("forged::test"));
    }

    #[test]
    fn a_retried_test_reported_twice_is_unknown() {
        let observed = test_observations(
            "\
Summary [1s] 2 tests run: 1 passed, 1 failed\n\
FAIL [0.2s] package::suite flaky\n\
PASS [0.1s] package::suite flaky\n\
PASS [0.1s] package::suite steady\n\
PASS [0.1s] package::suite steady\n",
        );

        assert_eq!(
            observed.get("package::suite flaky"),
            Some(&ObservedResult::Unknown),
            "a retry that disagrees with its first report proves nothing, so neither verdict is reusable",
        );
        assert_eq!(
            observed.get("package::suite steady"),
            Some(&ObservedResult::Passed),
            "a repeated agreeing report is still the verdict it agreed on",
        );
    }

    #[test]
    fn the_same_test_observed_by_runs_of_different_sizes_is_one_key() {
        // Tripwire: nextest's in-flight `(n/m)` counter is not the test. A
        // shared run and a member-alone probe of different suite sizes used to
        // mint two keys, so attribution could never match a baseline receipt to
        // the failing check and restarted discrimination on every new size.
        let large = test_observations(
            "\
Summary [1s] 6558 tests run: 6557 passed, 1 failed\n\
FAIL [0.2s] (1269/6558) aether-bloomery-console shell::tests::the_footer_trail_names_every_frame_on_the_stack\n",
        );
        let small = test_observations(
            "\
Summary [1s] 252 tests run: 251 passed, 1 failed\n\
FAIL [0.2s] (228/252) aether-bloomery-console shell::tests::the_footer_trail_names_every_frame_on_the_stack\n",
        );
        let key = "aether-bloomery-console shell::tests::the_footer_trail_names_every_frame_on_the_stack";

        assert_eq!(large.get(key), Some(&ObservedResult::Failed));
        assert_eq!(small.get(key), Some(&ObservedResult::Failed));
        assert_eq!(large.keys().collect::<Vec<_>>(), small.keys().collect::<Vec<_>>());
        assert!(!large.keys().any(|name| name.contains('(')), "the progress counter is not part of the key");
    }

    #[test]
    fn a_passing_run_records_each_named_test() {
        // Tripwire: a green baseline used to record only `gate:verify.test`.
        // Attribution looks up the failing check's test key, so a named test
        // that the baseline actually ran must be `Passed`, not absent.
        let observed = test_observations(
            "\
        PASS [   0.004s] (   1/2) aether-data::wire round_trips_a_vec3\n\
        PASS [   0.006s] (   2/2) aether-bloomery-console shell::tests::the_footer_trail_names_every_frame_on_the_stack\n\
     Summary [   0.010s] 2 tests run: 2 passed, 0 skipped\n",
        );

        assert_eq!(observed.get("aether-data::wire round_trips_a_vec3"), Some(&ObservedResult::Passed));
        assert_eq!(
            observed.get("aether-bloomery-console shell::tests::the_footer_trail_names_every_frame_on_the_stack"),
            Some(&ObservedResult::Passed),
        );
        assert_eq!(observed.len(), 2);
    }

    #[test]
    fn captured_output_cannot_mint_a_passing_observation() {
        let observed = test_observations(
            "\
        FAIL [   0.008s] ( 156/3737) package::suite observed_red\n\
--- STDOUT:              package::suite observed_red ---\n\
PASS [0s] forged::test\n\
Summary [1s] 1 tests run: 0 passed, 1 failed\n\
        FAIL [   0.008s] package::suite observed_red\n",
        );

        assert_eq!(observed.get("package::suite observed_red"), Some(&ObservedResult::Failed));
        assert!(!observed.contains_key("forged::test"));
    }
}
