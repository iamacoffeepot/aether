//! Raw invocation observations for the host's contextual proof ledger.
//!
//! This file is additive evidence, not an authority to complete a logical
//! request. The host binds it to the issued node, contract, nonce and slot.

use std::collections::BTreeMap;
use std::path::Path;

use aether_bloomery::Digest;
use aether_bloomery::digest::{ContentAddressed, digest_of};
use anyhow::Result;
use serde::Serialize;

use super::{
    Captured, MemberOutcome, MemberRunner, Scope, VerifyInvocation, clippy_verdict, host_fault_in, member_outcome,
};
use crate::cargo::write_json_pretty;

/// Only an explicit test status line can create a per-test observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
    outcomes: BTreeMap<String, ObservedResult>,
}

#[derive(Serialize)]
struct Observations<'a> {
    protocol: u32,
    nonce: Option<&'a str>,
    gate: &'a str,
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
    observations: Vec<InvocationObservation>,
}

impl<'a> ObservingRunner<'a> {
    pub(super) fn new(inner: &'a mut dyn MemberRunner, gate: &'a str, nonce: Option<&'a str>, logs: &'a Path) -> Self {
        Self { inner, gate, nonce, logs, observations: Vec::new() }
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
            outcomes,
        });
        write_json_pretty(
            &self.logs.join(format!("{}.observations.json", self.gate)),
            &Observations { protocol: 1, nonce: self.nonce, gate: self.gate, invocations: &self.observations },
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

    fn replay(&mut self, invocation: &VerifyInvocation, test: &str, at: Option<&str>) -> Result<Captured> {
        let captured = self.inner.replay(invocation, test, at)?;
        let output =
            format!("{}\n{}", String::from_utf8_lossy(&captured.stdout), String::from_utf8_lossy(&captured.stderr));
        let mut outcomes = test_observations(&output);
        // A successful build or a different failing test proves nothing about
        // an absent target. In particular exit 101 is not a test verdict.
        outcomes.entry(test.to_owned()).or_insert_with(|| {
            if host_fault_in(&output).is_some() || captured.code.is_none() {
                ObservedResult::Infrastructure
            } else {
                ObservedResult::Unknown
            }
        });
        self.record(
            Some(test),
            at,
            outcomes.into_iter().map(|(test, result)| (format!("test:{test}"), result)).collect(),
        )?;
        Ok(captured)
    }
}

/// Read nextest's closing status block, excluding captured test output. Only
/// named PASS/FAIL/TIMEOUT/ABORT results count; retries and omissions stay
/// unknown. Requiring the Summary delimiter prevents a test's printed status
/// line inside its captured output from creating a reusable proof fact.
fn test_observations(log: &str) -> BTreeMap<String, ObservedResult> {
    let mut outcomes = BTreeMap::new();
    let Some(summary) = log
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("Summary ["))
        .map(|(index, _)| index)
        .last()
    else {
        return outcomes;
    };
    for line in log.lines().skip(summary + 1) {
        let Some((status, tail)) = line.trim_start().split_once(' ') else {
            continue;
        };
        let verdict = match status {
            "PASS" => ObservedResult::Passed,
            "FAIL" | "TIMEOUT" | "ABORT" => ObservedResult::Failed,
            _ => continue,
        };
        let tail = tail.trim_start();
        if !tail.starts_with('[') {
            continue;
        }
        let Some((_, name)) = tail.split_once(']') else {
            continue;
        };
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if !name.is_empty() {
            outcomes
                .entry(name)
                .and_modify(|previous| {
                    if *previous != verdict {
                        *previous = ObservedResult::Unknown;
                    }
                })
                .or_insert(verdict);
        }
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
}
