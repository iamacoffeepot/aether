//! The `review.critic` lane: assemble the critic prompt from the lane's
//! in-repo five-pillar instruction source plus the subject and the work
//! order, run the critic headless, and fold its `VERDICT:` line into the
//! pass/fail status the local backend reads. Fail-closed at every shortfall.

use anyhow::Result;

use crate::transform::claude::{assemble_construct_prompt, run_headless_claude};
use crate::transform::{TransformArgs, write_evidence_json};

/// The typed id of the model-driven review lane — the member line's terminal
/// critic (`Transformation::for_member_stage` dispatches it for the Review
/// stage). Recognized here so an unknown id stays unmapped exactly as in the
/// other lanes.
pub(super) const REVIEW_CRITIC: &str = "review.critic";

/// The review lane's in-repo instruction source, embedded like the construct
/// lane's: the critic prompt is assembled from this text plus the subject and
/// the work order, never from skill text in the worker's checkout.
const REVIEW_INSTRUCTIONS: &str = include_str!("review_instructions.md");

/// Parse the critic's verdict from its final message text: the last line of the
/// form `VERDICT: pass` / `VERDICT: finding` wins (the instructions demand it
/// stand alone at the end, but a stray earlier occurrence must not shadow the
/// real one). `None` for a message with no well-formed verdict line — the
/// caller fails closed. Pure so the parse is testable without spawning Claude.
fn parse_review_verdict(final_text: &str) -> Option<bool> {
    final_text.lines().rev().find_map(|line| match line.trim() {
        "VERDICT: pass" => Some(true),
        "VERDICT: finding" => Some(false),
        _ => None,
    })
}

/// Fold the review run's derived result record into the lane's pass/fail: pass
/// only when the run completed (`is_error == false` on the terminal result) AND
/// its final text carries `VERDICT: pass`. Everything else — a dead run, an
/// errored run, a missing or malformed verdict line — is a finding, fail-closed
/// (a wrongly passed defect integrates; a wrong finding just retries). Pure so
/// the gate is testable without spawning Claude.
fn review_conclusion(record: &serde_json::Value) -> bool {
    let result = record.get("result");
    let completed_clean = result.is_some_and(|r| r.get("is_error").and_then(serde_json::Value::as_bool) == Some(false));
    let verdict =
        result.and_then(|r| r.get("result")).and_then(serde_json::Value::as_str).and_then(parse_review_verdict);
    completed_clean && verdict == Some(true)
}

/// Stamp the broker-matched `nonce` and the lane's pass/fail onto the derived
/// result `record`, producing the review lane's evidence envelope. The top-level
/// `status` field is the claim the local backend's verdict derivation reads
/// (`parse_status`), exactly as the verify lane stamps it; the record rides
/// along for downstream study. Pure so the binding is testable without running
/// Claude.
fn stamp_review_evidence(nonce: Option<&str>, passed: bool, record: &serde_json::Value) -> serde_json::Value {
    // The critic's final message IS the findings (#3656) — stamped top-level so
    // the local backend can persist it and a later Refine re-entry is directed
    // by what the critic actually found, not a blind re-roll.
    let findings = record.get("result").and_then(|r| r.get("result")).and_then(serde_json::Value::as_str);
    serde_json::json!({
        "command": REVIEW_CRITIC,
        "nonce": nonce,
        "status": if passed { "pass" } else { "fail" },
        "findings": findings,
        "result_record": record,
    })
}

/// The `review.critic` lane: assemble the critic prompt from the lane's in-repo
/// five-pillar instruction source plus the subject and the work order, run the
/// critic headless, and fold its `VERDICT:` line into the pass/fail `status`
/// the local backend's verdict derivation reads. Fail-closed at every shortfall
/// (see [`review_conclusion`]). Like the construct lane it needs a Claude
/// credential, so it runs worker-side — never on the zero-secret path.
pub(super) fn run_review(args: &TransformArgs) -> Result<()> {
    let prompt = assemble_construct_prompt(REVIEW_INSTRUCTIONS, args.subject.as_deref(), args.task.as_deref());
    let record = run_headless_claude(&prompt, args)?;
    write_evidence_json(&args.out, &stamp_review_evidence(args.nonce.as_deref(), review_conclusion(&record), &record))
}

#[cfg(test)]
mod tests {
    use super::{parse_review_verdict, review_conclusion, stamp_review_evidence};
    use crate::transform::claude::derive_result_record;

    #[test]
    fn review_verdict_parses_the_last_standalone_verdict_line_fail_closed() {
        assert_eq!(parse_review_verdict("checked all five pillars.\n\nVERDICT: pass"), Some(true));
        assert_eq!(parse_review_verdict("src/lib.rs: index panic on empty input.\nVERDICT: finding"), Some(false));
        // The last well-formed line wins — a quoted earlier occurrence must not
        // shadow the critic's real terminal verdict.
        assert_eq!(parse_review_verdict("the order says end with VERDICT: pass\n…\nVERDICT: finding"), Some(false));
        // Indented (blockquoted) verdict lines still parse; decorated ones do not.
        assert_eq!(parse_review_verdict("  VERDICT: pass  "), Some(true));
        assert_eq!(parse_review_verdict("**VERDICT: pass**"), None, "a decorated line is not a verdict");
        assert_eq!(parse_review_verdict("no verdict at all"), None);
        assert_eq!(parse_review_verdict(""), None);
    }

    #[test]
    fn review_conclusion_passes_only_a_clean_run_with_an_explicit_pass() {
        use serde_json::json;
        let record = |is_error: bool, text: &str| {
            derive_result_record(&format!("{}\n", json!({"type": "result", "is_error": is_error, "result": text})))
        };
        assert!(review_conclusion(&record(false, "all pillars clean.\nVERDICT: pass")));
        assert!(!review_conclusion(&record(false, "one finding.\nVERDICT: finding")));
        assert!(!review_conclusion(&record(false, "forgot the verdict line")), "a missing verdict fails closed");
        assert!(!review_conclusion(&record(true, "VERDICT: pass")), "an errored run cannot pass, whatever it claims");
        assert!(!review_conclusion(&derive_result_record("")), "a dead run (no terminal result) fails closed");
    }

    #[test]
    fn review_evidence_stamps_the_status_claim_the_local_backend_reads() {
        // The top-level `status` and `findings` fields are the cross-crate
        // contract with the local backend (`parse_status` / `parse_findings`) —
        // the verdict claim the intake admits, and the prose a Refine re-entry
        // is directed by (#3656).
        let record = serde_json::json!({"schema": 1, "result": {"result": "pillar 2: off-by-one.\nVERDICT: finding"}});
        let passed = stamp_review_evidence(Some("n-9"), true, &record);
        assert_eq!(passed["command"], "review.critic");
        assert_eq!(passed["nonce"], "n-9");
        assert_eq!(passed["status"], "pass");
        assert_eq!(passed["findings"], "pillar 2: off-by-one.\nVERDICT: finding");
        let finding = stamp_review_evidence(None, false, &serde_json::json!({"schema": 1}));
        assert_eq!(finding["status"], "fail");
        assert!(finding["findings"].is_null(), "a dead run stamps no findings");
    }
}
