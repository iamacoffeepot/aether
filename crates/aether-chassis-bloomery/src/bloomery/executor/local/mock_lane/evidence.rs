//! The `evidence.json` a mock run leaves, and what it leaves in the worktree.
//!
//! The bodies here are the contract with the coordinator's own readers, not an
//! invention: `parse_status`, `parse_findings`, `parse_cost`, and
//! `construct_conclusion` in the local backend are what read them, and the real
//! lane's three shapes are what they mirror —
//!
//! - a verify lane stamps `status` / `exit_code` / `log`, plus `findings` on a
//!   failure;
//! - a review lane stamps `status` / `findings` / `result_record`;
//! - a construct lane stamps `produced_candidate` / `result_record` and no
//!   status at all, and leaves its candidate as working-tree changes for the
//!   coordinator to capture.
//!
//! A mock that got these wrong would exercise the spawn and then hand the
//! coordinator something it never sees in production, which is the one way this
//! tier could be green and worthless.

use std::path::Path;
use std::{fs, io};

use aether_bloomery::{CONSTRUCT_IMPLEMENT_COMMAND, REVIEW_CRITIC_COMMAND, VerifyFailure, VerifyFailureSet};
use serde_json::{Value, json};

use super::script::LaneMode;

/// The file a passing construct run writes into the scratch worktree, so the
/// coordinator's `git status --porcelain` sees a candidate to capture.
pub const CANDIDATE_FILE: &str = "mock-lane-candidate.txt";

/// What a run does once its mode is chosen: the bytes to write (or not), and the
/// status to exit with.
pub struct Outcome {
    /// The `evidence.json` bytes, or `None` to write no file at all.
    pub evidence: Option<Vec<u8>>,
    /// The process exit code.
    pub exit_code: i32,
    /// The candidate body to write into the worktree, or `None` to leave it
    /// clean.
    ///
    /// Stamped with the run's nonce rather than fixed, because the coordinator
    /// detects a candidate with `git status --porcelain`: a `Refine` re-entry
    /// checks out the *previous* candidate, so a lane that rewrote identical
    /// bytes would leave a clean tree and have its capture fail closed. A repair
    /// lap has to change something, exactly as a real one does.
    pub candidate: Option<String>,
}

/// The `result_record` a model lane nests — a terminal, non-errored run with
/// plausible cost columns, which is what `construct_conclusion` gates on and
/// what `parse_study_cost` reads.
///
/// The columns are small but non-zero so a study row is distinguishable from an
/// unmeasured attempt; `cost_usd` is deliberately present and deliberately
/// ignored downstream (price comes from a sealed table, not the lane's claim).
fn result_record(is_error: bool, final_text: Option<&str>) -> Value {
    json!({
        "schema": 1,
        "is_error": is_error,
        "num_turns": 3,
        "duration_ms": 1_200,
        "input": 900,
        "cache_write": 100,
        "cache_read": 4_000,
        "output": 250,
        "cost_usd": 0.01,
        "result": { "is_error": is_error, "result": final_text },
    })
}

/// The diagnostics a failing mechanical lane distils — shaped like a compiler's,
/// because a `Refine` re-entry is directed by this text and a scenario asserting
/// the loop should carry something a reader recognizes as the reason.
fn verify_findings(command: &str) -> String {
    format!(
        "{command} failed.\n\nerror[E0308]: mismatched types\n  --> crates/mock/src/lib.rs:7:20\n   |\n 7 |     let total: u32 = \
         count();\n   |                --- ^^^^^^^ expected `u32`, found `usize`\n"
    )
}

/// The report an `environment` verdict carries: a ground step that could not
/// execute, which is a host fault rather than a judgement on the candidate.
fn environment_findings() -> String {
    "the review could not run: the sandbox refused to start.\n\nVERDICT: environment".to_owned()
}

/// Decide what a run of `command` under `mode` does.
///
/// `nonce` is stamped into the evidence the way the real lanes stamp theirs.
#[must_use]
pub fn outcome(command: &str, nonce: &str, mode: LaneMode) -> Outcome {
    let body = |value: &Value| Some(serde_json::to_vec_pretty(value).unwrap_or_default());
    let evidence_nonce = if mode == LaneMode::MismatchedNonce {
        "mismatched-nonce"
    } else {
        nonce
    };

    match mode {
        LaneMode::NoEvidence => return Outcome { evidence: None, exit_code: 0, candidate: None },
        LaneMode::ExitsNonZero => return Outcome { evidence: None, exit_code: 2, candidate: None },
        LaneMode::EmptyEvidence => return Outcome { evidence: Some(Vec::new()), exit_code: 0, candidate: None },
        LaneMode::MalformedEvidence => {
            return Outcome { evidence: Some(b"{not json".to_vec()), exit_code: 0, candidate: None };
        }
        // Handled per lane family below.
        LaneMode::Pass
        | LaneMode::Fail
        | LaneMode::Environment
        | LaneMode::ConcludesWithoutWriting
        | LaneMode::MismatchedNonce
        | LaneMode::NeverExits => {}
    }

    if command == CONSTRUCT_IMPLEMENT_COMMAND {
        // The construct lane's only claim is whether it produced a candidate;
        // it stamps no status, so the coordinator's construct gate is what
        // decides. `ConcludesWithoutWriting` is the interesting one: it claims a
        // candidate and leaves the tree clean, which the capture must catch.
        let produced = matches!(
            mode,
            LaneMode::Pass | LaneMode::ConcludesWithoutWriting | LaneMode::MismatchedNonce | LaneMode::NeverExits
        );
        return Outcome {
            evidence: body(&json!({
                "command": command,
                "nonce": evidence_nonce,
                "produced_candidate": produced,
                "result_record": result_record(false, Some("wrote the candidate.")),
            })),
            exit_code: 0,
            candidate: matches!(mode, LaneMode::Pass | LaneMode::MismatchedNonce | LaneMode::NeverExits)
                .then(|| format!("the candidate a mock construct lane left for run {nonce}.\n")),
        };
    }

    if command == REVIEW_CRITIC_COMMAND {
        let passed = matches!(mode, LaneMode::Pass | LaneMode::MismatchedNonce | LaneMode::NeverExits);
        let findings = match mode {
            LaneMode::Environment => Value::String(environment_findings()),
            _ if passed => Value::Null,
            _ => Value::String(
                "pillar 2: the candidate reintroduces the bug it claims to fix.\nVERDICT: finding".to_owned(),
            ),
        };
        // Three statuses, exactly as the real lane stamps them (ADR-0176): an
        // `environment` run judged no candidate, so it is neither the pass it
        // cannot claim nor the fail a candidate could repair.
        let status = match mode {
            LaneMode::Environment => "environment",
            _ if passed => "pass",
            _ => "fail",
        };
        return Outcome {
            evidence: body(&json!({
                "command": command,
                "nonce": evidence_nonce,
                "status": status,
                "findings": findings,
                "result_record": result_record(false, findings.as_str()),
            })),
            exit_code: 0,
            candidate: None,
        };
    }

    // Every remaining command is a mechanical verify lane.
    let passed = matches!(
        mode,
        LaneMode::Pass | LaneMode::ConcludesWithoutWriting | LaneMode::MismatchedNonce | LaneMode::NeverExits
    );
    let mut evidence = json!({
        "command": command,
        "nonce": evidence_nonce,
        "status": if passed { "pass" } else { "fail" },
        "exit_code": i32::from(!passed),
        "log": format!("{command}.log"),
    });
    if !passed && let Some(object) = evidence.as_object_mut() {
        object.insert("failed_verifiers".to_owned(), json!(VerifyFailureSet::one(VerifyFailure::Clippy)));
        object.insert("findings".to_owned(), Value::String(verify_findings(command)));
    }

    Outcome { evidence: body(&evidence), exit_code: i32::from(!passed), candidate: None }
}

/// Apply an outcome: write the candidate into `worktree` when there is one, and
/// the evidence into `out` when there is any.
///
/// # Errors
/// A directory could not be created or a file could not be written.
pub fn apply(outcome: &Outcome, worktree: &Path, out: &Path) -> io::Result<()> {
    if let Some(candidate) = &outcome.candidate {
        fs::write(worktree.join(CANDIDATE_FILE), candidate)?;
    }
    if let Some(evidence) = &outcome.evidence {
        fs::create_dir_all(out)?;
        fs::write(out.join("evidence.json"), evidence)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a fixture asserting on evidence it just built reports a miss by panicking")]
mod tests {
    use aether_bloomery::{
        CONSTRUCT_IMPLEMENT_COMMAND, REVIEW_CRITIC_COMMAND, VERIFY_CHECK_COMMAND, VerifyFailure, VerifyFailureSet,
    };
    use serde_json::Value;

    use super::super::script::LaneMode;
    use super::outcome;

    fn decoded(command: &str, mode: LaneMode) -> Value {
        serde_json::from_slice(&outcome(command, "n-1", mode).evidence.unwrap()).unwrap()
    }

    #[test]
    fn a_construct_run_stamps_the_candidate_claim_and_no_status() {
        // Tripwire: the local backend routes a construct lane through
        // `construct_conclusion` (`produced_candidate` AND
        // `result_record.is_error == false`) and never through `parse_status`.
        // Evidence that stamped a status would exercise the wrong gate — the
        // mock would be testing a path production never takes.
        let evidence = decoded(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::Pass);

        assert_eq!(evidence["produced_candidate"], Value::Bool(true));
        assert_eq!(evidence["result_record"]["is_error"], Value::Bool(false));
        assert!(evidence.get("status").is_none(), "a construct lane stamps no status");
    }

    #[test]
    fn a_construct_run_that_writes_nothing_still_claims_a_candidate() {
        // Tripwire: this mode exists to make the *capture* fail the run, not the
        // evidence. If the claim went false, the construct gate would already
        // refuse and the scenario would never reach the clean-worktree
        // downgrade it is there to exercise.
        let claimed = decoded(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::ConcludesWithoutWriting);

        assert_eq!(claimed["produced_candidate"], Value::Bool(true));
        assert!(
            outcome(CONSTRUCT_IMPLEMENT_COMMAND, "n-1", LaneMode::ConcludesWithoutWriting).candidate.is_none(),
            "the whole mode is a claim with nothing behind it",
        );
    }

    #[test]
    fn an_environment_verdict_stamps_its_own_status_rather_than_a_review_failure() {
        // Tripwire: this mock's whole job is to present what production presents.
        // While it stamped `fail`, the executor could only ever derive a failing
        // review from it, so every scenario built on this mode would exercise the
        // candidate-blaming path the fix exists to remove and pass regardless.
        let evidence = decoded(REVIEW_CRITIC_COMMAND, LaneMode::Environment);

        assert_eq!(evidence["status"], Value::String("environment".to_owned()));
        assert!(evidence["findings"].as_str().unwrap().contains("VERDICT: environment"));
    }

    #[test]
    fn a_failing_mechanical_lane_carries_diagnostics_and_a_non_zero_exit() {
        // Tripwire: the failing verify lane's findings are what a `Refine`
        // re-entry is steered by, and its non-zero exit is what the backend's
        // lifecycle reads. A mock that passed the status but exited zero would
        // make the two disagree in a way production never does.
        let run = outcome(VERIFY_CHECK_COMMAND, "n-1", LaneMode::Fail);
        let evidence: Value = serde_json::from_slice(&run.evidence.clone().unwrap()).unwrap();

        assert_eq!(evidence["status"], Value::String("fail".to_owned()));
        assert_eq!(
            serde_json::from_value::<VerifyFailureSet>(evidence["failed_verifiers"].clone()).unwrap(),
            VerifyFailureSet::one(VerifyFailure::Clippy),
        );
        assert!(evidence["findings"].as_str().unwrap().contains("E0308"));
        assert_eq!(run.exit_code, 1);
    }

    #[test]
    fn two_construct_runs_leave_distinguishable_candidates() {
        // Tripwire: a `Refine` re-entry checks out the previous candidate, so a
        // lane that rewrote identical bytes leaves `git status --porcelain`
        // empty and the capture fails closed — the repair lap silently becomes a
        // failed attempt, and a member that should land wedges instead.
        let first = outcome(CONSTRUCT_IMPLEMENT_COMMAND, "n-1", LaneMode::Pass).candidate;
        let second = outcome(CONSTRUCT_IMPLEMENT_COMMAND, "n-2", LaneMode::Pass).candidate;

        assert!(first.is_some() && first != second, "a repair lap has to change something: {first:?} vs {second:?}");
    }

    #[test]
    fn the_shortfall_modes_leave_exactly_what_they_name() {
        assert!(outcome(VERIFY_CHECK_COMMAND, "n-1", LaneMode::NoEvidence).evidence.is_none());
        assert_eq!(outcome(VERIFY_CHECK_COMMAND, "n-1", LaneMode::EmptyEvidence).evidence, Some(Vec::new()));
        assert_eq!(outcome(VERIFY_CHECK_COMMAND, "n-1", LaneMode::ExitsNonZero).exit_code, 2);
        assert!(
            serde_json::from_slice::<Value>(
                &outcome(VERIFY_CHECK_COMMAND, "n-1", LaneMode::MalformedEvidence).evidence.unwrap()
            )
            .is_err(),
            "malformed evidence must not decode, or the scenario reads as a plain verdict",
        );
    }
}
