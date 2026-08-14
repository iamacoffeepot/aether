//! The `review.critic` lane: assemble the critic prompt from the lane's
//! in-repo five-pillar instruction source plus the subject and the work
//! order, run the critic headless, and fold its `VERDICT:` line into the
//! pass/fail status the local backend reads. Fail-closed at every shortfall.

use std::path::Path;

use anyhow::Result;

use crate::transform::claude::assemble_construct_prompt;
use crate::transform::{Measurements, TransformArgs, conventions, run_model_lane, write_evidence_json};

/// The typed id of the model-driven review lane — the member line's terminal
/// critic (`Transformation::for_member_stage` dispatches it for the Review
/// stage). Recognized here so an unknown id stays unmapped exactly as in the
/// other lanes.
pub(super) const REVIEW_CRITIC: &str = "review.critic";

/// The review lane's in-repo instruction source, embedded like the construct
/// lane's: the critic prompt is assembled from this text plus the subject and
/// the work order, never from skill text in the worker's checkout.
const REVIEW_INSTRUCTIONS: &str = include_str!("review_instructions.md");

/// What a critic's final message claims. `Environment` is not a judgment of the
/// candidate at all: it is the critic reporting that the ground step naming the
/// candidate could not execute, so there was nothing to judge (#4723).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReviewVerdict {
    Pass,
    Finding,
    Environment,
}

/// Parse the critic's verdict from its final message text: the last line of the
/// form `VERDICT: pass` / `VERDICT: finding` / `VERDICT: environment` wins (the
/// instructions demand it stand alone at the end, but a stray earlier occurrence
/// must not shadow the real one). `None` for a message with no well-formed
/// verdict line — the caller fails closed. Pure so the parse is testable without
/// spawning Claude.
fn parse_review_verdict(final_text: &str) -> Option<ReviewVerdict> {
    final_text.lines().rev().find_map(|line| match line.trim() {
        "VERDICT: pass" => Some(ReviewVerdict::Pass),
        "VERDICT: finding" => Some(ReviewVerdict::Finding),
        "VERDICT: environment" => Some(ReviewVerdict::Environment),
        _ => None,
    })
}

/// The `## Candidate` section of the assembled prompt: the exact commands that
/// show the candidate this run judges, composed from the work order's diff
/// source rather than assumed by the instruction text (#4723).
///
/// A member's candidate is the uncommitted change its construct lane left in the
/// working tree. An aggregate review's is the integration the fold already
/// committed, so a working-tree diff there is empty on every run and the lane's
/// own empty-diff rule made every aggregate review a mandatory finding. Naming
/// the source here keeps one instruction text honest for both, rather than
/// giving the critic a stage flag to branch its own reading on.
fn candidate_section(diff_base: Option<&str>) -> String {
    diff_base.map_or_else(
        || {
            String::from(
                "\n## Candidate\n\nThe candidate is **uncommitted**: it is the change the working tree carries. \
                 Show it with `git status --porcelain` and `git diff HEAD`.\n",
            )
        },
        |base| {
            format!(
                "\n## Candidate\n\nThe candidate is **committed**: it is everything the range `{base}..HEAD` \
                 carries. Show it with `git diff {base}..HEAD`, and read the commits it spans with \
                 `git log --oneline {base}..HEAD`. The working tree is a clean checkout of that range's head, \
                 so `git diff HEAD` is empty here and says nothing about the candidate.\n"
            )
        },
    )
}

/// The `## Composition review` section, present only when the order names a diff
/// base (ADR-0191 §3).
///
/// The diff base is what tells the two reviews apart, and it is not an incidental
/// signal: a member's candidate is the uncommitted change its lane left behind,
/// while the composition's candidate is the committed weave the fold produced. So
/// the order that names a range is exactly the order whose subject is the weave.
///
/// The contract this states is the whole point of ADR-0191. The composition
/// review is not a second pass over the members: they have each already passed
/// their own review, they are immutable, and re-reading their diffs here is what
/// turned one reviewer's attention sampling into bloom-wide coupling (bloom
/// `10a1228c` spent three judge rounds and eight of nine findings that way). The
/// question is narrower and answerable: did each member's intent survive the
/// weaving?
fn composition_contract(diff_base: Option<&str>) -> String {
    let Some(base) = diff_base else {
        return String::new();
    };

    format!(
        "\n## Composition review\n\n\
         This is a **composition review**. The candidate is the *weave*: the fold of several members' \
         already-reviewed candidates, plus every edit authored at a seam where they collided. Each member \
         passed its own review before it entered this tree, and each is finished and immutable. Your subject \
         is the weaving, not the members.\n\n\
         Judge exactly three things:\n\n\
         1. **The seam edits.** Every change in the range that no member authored — the reconciliation work. \
         Read these in full, on all five pillars below.\n\
         2. **The files more than one member touched.** Find them with \
         `git log --format='%H' {base}..HEAD | while read c; do git diff-tree --no-commit-id --name-only -r \"$c\"; \
         done | sort | uniq -d`. A file two members changed is where one intent can quietly overwrite another.\n\
         3. **Per-member acceptance.** For each work order in the `## Task` section, check that what it promised \
         is still visibly present in the composed tree. This is a presence check against the order, not a re-review \
         of how the member implemented it.\n\n\
         **Do not re-read the member diffs.** The member work orders and candidates are reference input — they \
         tell you what each member set out to do, so you can tell whether the weave preserved it. A defect in a \
         member's own code that the weave faithfully carried through is *not* a finding of this review: it belongs \
         to a member that is already done, and it is filed as new work rather than reopening finished work. If you \
         see one and it is serious, name it in your justification prose as an observation and say plainly that it \
         is member-scope; do not let it decide your verdict.\n\n\
         Findings freeze per subject, exactly as member review findings do: on a re-review of a repaired weave, \
         discharge the frozen findings you were given and judge only what changed. Do not open a fresh full pass \
         over work you already judged.\n"
    )
}

/// The findings prose an `environment` verdict produces: the critic's own report
/// under an operator-directed framing.
///
/// Shaped like the verify lane's missing-tool findings, and for the same reason —
/// the reader of a failed review is a member's `Refine` re-entry, and directing
/// one at a host that could not run the ground step spends a model attempt to
/// learn nothing.
fn environment_findings(report: &str) -> String {
    format!(
        "The review did not run. The critic could not execute the ground step that shows the candidate, so \
         it never judged one — which is not the same as the candidate failing, and no change to the \
         candidate can fix it.\n\n{report}\n\nRepair the executor host and re-dispatch."
    )
}

/// The critic's final message text, if the run reached one.
fn final_text(record: &serde_json::Value) -> Option<&str> {
    record.get("result").and_then(|result| result.get("result")).and_then(serde_json::Value::as_str)
}

/// Fold the review run's derived result record into the lane's terminal verdict:
/// the critic's own `VERDICT:` line, but only from a run that completed
/// (`is_error == false` on the terminal result). Everything else — a dead run,
/// an errored run, a missing or malformed verdict line — folds to `Finding`,
/// fail-closed (a wrongly passed defect integrates; a wrong finding just
/// retries).
///
/// `Environment` survives the fold rather than collapsing into `Finding`
/// (ADR-0176): the critic judged no candidate, so the result is a claim about
/// the host, and flattening it here is what charged a member repair lap for an
/// executor outage. It is still gated on a clean completion — a run that died
/// cannot be trusted to have reported why. Pure so the gate is testable without
/// spawning Claude.
fn review_conclusion(record: &serde_json::Value) -> ReviewVerdict {
    let completed_clean = record
        .get("result")
        .is_some_and(|result| result.get("is_error").and_then(serde_json::Value::as_bool) == Some(false));

    if !completed_clean {
        return ReviewVerdict::Finding;
    }
    final_text(record).and_then(parse_review_verdict).unwrap_or(ReviewVerdict::Finding)
}

/// Stamp the broker-matched `nonce` and the lane's terminal `verdict` onto the
/// derived result `record`, producing the review lane's evidence envelope. The
/// top-level `status` field is the claim the local backend's verdict derivation
/// reads (`parse_status`), exactly as the verify lane stamps it; the record
/// rides along for downstream study. Pure so the binding is testable without
/// running Claude.
///
/// Three stamped statuses, not two: an `environment` verdict is a claim that no
/// candidate was judged, and it stays distinct through the envelope so the
/// executor can raise it as a host fault rather than a failing review
/// (ADR-0176).
fn stamp_review_evidence(
    nonce: Option<&str>,
    verdict: ReviewVerdict,
    record: &serde_json::Value,
    measured: Measurements,
) -> serde_json::Value {
    // The critic's final message IS the findings (#3656) — stamped top-level so
    // the local backend can persist it and a later Refine re-entry is directed
    // by what the critic actually found, not a blind re-roll. An `environment`
    // report is framed as the host fault it is, so the re-entry's reader is not
    // handed a candidate defect that was never found (#4723).
    let findings = final_text(record).map(|report| match parse_review_verdict(report) {
        Some(ReviewVerdict::Environment) => environment_findings(report),
        _ => report.to_owned(),
    });
    let mut evidence = serde_json::json!({
        "command": REVIEW_CRITIC,
        "nonce": nonce,
        "status": status_token(verdict),
        "findings": findings,
        "result_record": record,
    });
    measured.stamp(&mut evidence);
    evidence
}

/// The stamped `status` a terminal verdict produces. `finding` stamps `fail`
/// because the envelope's status is the executor's verdict channel, not the
/// critic's vocabulary — a finding is the failing review it has always been.
fn status_token(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::Pass => "pass",
        ReviewVerdict::Finding => "fail",
        ReviewVerdict::Environment => "environment",
    }
}

/// The `review.critic` lane: assemble the critic prompt from the lane's in-repo
/// five-pillar instruction source plus the subject and the work order, run the
/// critic headless, and fold its `VERDICT:` line into the pass/fail `status`
/// the local backend's verdict derivation reads. Fail-closed at every shortfall
/// (see [`review_conclusion`]). Like the construct lane it needs a Claude
/// credential, so it runs worker-side — never on the zero-secret path.
pub(super) fn run_review(args: &TransformArgs) -> Result<()> {
    // Pillars 3 and 5 judge the candidate against the repository's stated
    // conventions, so the critic is given them rather than told to go and read
    // them (#4647) — a critic without the rules cannot cite the one a candidate
    // broke, and passes convention drift by default.
    //
    // The instruction text arrives with the `## Candidate` section the order's
    // diff source composes (#4723): what the critic runs to see the candidate is
    // a property of the work order, not something the prompt asks it to infer.
    // The same signal carries the composition contract (ADR-0191 §3) — an order
    // whose candidate is a committed range is the composition's review, and its
    // subject is the weave rather than the members.
    let prompt = assemble_construct_prompt(
        &format!(
            "{REVIEW_INSTRUCTIONS}{}{}",
            candidate_section(args.diff_base.as_deref()),
            composition_contract(args.diff_base.as_deref()),
        ),
        conventions::read(Path::new(".")).as_deref(),
        args.subject.as_deref(),
        args.task.as_deref(),
    );
    let run = run_model_lane(&prompt, args)?;
    write_evidence_json(
        &args.out,
        &stamp_review_evidence(args.nonce.as_deref(), review_conclusion(&run.record), &run.record, run.measured),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        Measurements, ReviewVerdict, candidate_section, composition_contract, parse_review_verdict, review_conclusion,
        stamp_review_evidence,
    };
    use crate::transform::messages::derive_result_record;

    #[test]
    fn review_verdict_parses_the_last_standalone_verdict_line_fail_closed() {
        use ReviewVerdict::{Environment, Finding, Pass};

        assert_eq!(parse_review_verdict("checked all five pillars.\n\nVERDICT: pass"), Some(Pass));
        assert_eq!(parse_review_verdict("src/lib.rs: index panic on empty input.\nVERDICT: finding"), Some(Finding));
        // A ground step that could not execute is its own terminal claim, not a
        // finding: the critic judged no candidate, so the prose a Refine is
        // directed by must not read as a defect it found (#4723).
        assert_eq!(parse_review_verdict("bwrap: loopback failed.\nVERDICT: environment"), Some(Environment));
        // The last well-formed line wins — a quoted earlier occurrence must not
        // shadow the critic's real terminal verdict.
        assert_eq!(parse_review_verdict("the order says end with VERDICT: pass\n…\nVERDICT: finding"), Some(Finding));
        // Indented (blockquoted) verdict lines still parse; decorated ones do not.
        assert_eq!(parse_review_verdict("  VERDICT: pass  "), Some(Pass));
        assert_eq!(parse_review_verdict("**VERDICT: pass**"), None, "a decorated line is not a verdict");
        assert_eq!(parse_review_verdict("no verdict at all"), None);
        assert_eq!(parse_review_verdict(""), None);
    }

    #[test]
    fn a_diff_base_makes_the_committed_range_the_candidate() {
        // Tripwire: the aggregate review checks out the integration *commit*, so
        // the working-tree diff the member contract names is empty on every run —
        // and the lane's own empty-diff rule then made every aggregate review a
        // mandatory finding (#4723). An order that names a diff base must direct
        // the critic at the range instead, and must not leave the working-tree
        // command standing next to it for the critic to run and believe.
        let ranged = candidate_section(Some("abc123"));

        assert!(ranged.contains("git diff abc123..HEAD"), "the range is the candidate: {ranged}");
        assert!(!ranged.contains("git status --porcelain"), "a committed candidate is not a working-tree probe");

        // The member contract is unchanged: no base named, the working tree is
        // the candidate, and an empty one stays a finding.
        let working_tree = candidate_section(None);

        assert!(working_tree.contains("git diff HEAD"));
        assert!(working_tree.contains("git status --porcelain"));
        assert!(!working_tree.contains(".."), "a working-tree candidate names no range");
    }

    #[test]
    fn only_a_composition_review_is_told_to_judge_the_weave() {
        // Tripwire (ADR-0191 §3): the composition review's whole cost argument is
        // that it does not re-read the member diffs. A contract that leaked onto
        // the member review would tell a member critic its subject is a weave
        // that does not exist there; a contract missing from the composition
        // review is the 10a1228c behaviour — a full re-read of every member,
        // eight member-scope findings out of nine, and three judge rounds.
        let composition = composition_contract(Some("abc123"));

        assert!(composition.contains("## Composition review"), "{composition}");
        assert!(composition.contains("Do not re-read the member diffs"), "{composition}");
        assert!(composition.contains("reference input"), "the member work orders are reference: {composition}");
        assert!(composition.contains("abc123..HEAD"), "the overlap probe spans the weave's range: {composition}");
        assert!(composition.contains("immutable"), "members are done: {composition}");
        assert!(composition.contains("freeze"), "findings discharge rather than re-open: {composition}");

        assert!(composition_contract(None).is_empty(), "a member review carries no composition contract");
    }

    #[test]
    fn review_conclusion_passes_only_a_clean_run_with_an_explicit_pass() {
        use ReviewVerdict::{Environment, Finding, Pass};
        use serde_json::json;
        let record = |is_error: bool, text: &str| {
            derive_result_record(&format!("{}\n", json!({"type": "result", "is_error": is_error, "result": text})))
        };
        assert_eq!(review_conclusion(&record(false, "all pillars clean.\nVERDICT: pass")), Pass);
        assert_eq!(review_conclusion(&record(false, "one finding.\nVERDICT: finding")), Finding);
        assert_eq!(
            review_conclusion(&record(false, "forgot the verdict line")),
            Finding,
            "a missing verdict fails closed"
        );
        assert_eq!(review_conclusion(&record(true, "VERDICT: pass")), Finding, "an errored run cannot pass");
        assert_eq!(
            review_conclusion(&derive_result_record("")),
            Finding,
            "a dead run (no terminal result) fails closed"
        );
        // Tripwire (ADR-0176): folding this to `Finding` is the regression —
        // the executor then reports a failing review, intake admits a candidate
        // verdict, and members that were never judged are charged a repair lap.
        assert_eq!(review_conclusion(&record(false, "the probe could not run.\nVERDICT: environment")), Environment);
        // But only from a run that concluded: a dead run's claim about the host
        // is no more trustworthy than its claim about the candidate.
        assert_eq!(review_conclusion(&record(true, "VERDICT: environment")), Finding);
    }

    #[test]
    fn review_evidence_stamps_the_status_claim_the_local_backend_reads() {
        // The top-level `status` and `findings` fields are the cross-crate
        // contract with the local backend (`parse_status` / `parse_findings`) —
        // the verdict claim the intake admits, and the prose a Refine re-entry
        // is directed by (#3656).
        let record = serde_json::json!({"schema": 1, "result": {"result": "pillar 2: off-by-one.\nVERDICT: finding"}});
        let passed = stamp_review_evidence(Some("n-9"), ReviewVerdict::Pass, &record, Measurements::default());
        assert_eq!(passed["command"], "review.critic");
        assert_eq!(passed["nonce"], "n-9");
        assert_eq!(passed["status"], "pass");
        assert_eq!(passed["findings"], "pillar 2: off-by-one.\nVERDICT: finding");
        let finding = stamp_review_evidence(
            None,
            ReviewVerdict::Finding,
            &serde_json::json!({"schema": 1}),
            Measurements::default(),
        );
        assert_eq!(finding["status"], "fail");
        assert!(finding["findings"].is_null(), "a dead run stamps no findings");
    }

    #[test]
    fn an_environment_verdict_stamps_its_own_status_and_names_the_host_fault() {
        // Tripwire (ADR-0176): `environment` is the executor-fault claim, and
        // stamping it as `fail` here is what made a host outage indistinguishable
        // from a critic's finding at every boundary downstream. The prose matters
        // too — it is what a reader of the fault is handed, so it has to say the
        // host could not run the check rather than name a defect nobody found.
        let record = serde_json::json!({
            "schema": 1,
            "result": {"result": "`git diff` failed: bwrap: loopback failed.\nVERDICT: environment"},
        });

        let stamped =
            stamp_review_evidence(Some("n-env"), ReviewVerdict::Environment, &record, Measurements::default());

        assert_eq!(stamped["status"], "environment", "a review that judged nothing reports neither pass nor fail");
        let findings = stamped["findings"].as_str().expect("an environment report stamps findings");
        assert!(findings.contains("bwrap: loopback failed"), "the critic's own report survives: {findings}");
        assert!(
            findings.contains("not the same as the candidate failing"),
            "the reader must be told this is a host fault, not a candidate one: {findings}",
        );
    }
}
