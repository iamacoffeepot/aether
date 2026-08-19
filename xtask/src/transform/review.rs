//! The `review.critic` lane: assemble the critic prompt from the lane's
//! in-repo five-pillar instruction source plus the subject and the work
//! order, run the critic headless, and fold its reports into the pass/fail
//! status the local backend reads. Fail-closed at every shortfall.
//!
//! The Claude harness injects append-only report tools and the status is
//! derived from the findings file. Harnesses without tool injection
//! (muse / grok) keep the terminal `VERDICT:` parse.

use aether_bloomery::Harness;
use anyhow::Result;

use crate::transform::claude::assemble_construct_prompt;
use crate::transform::messages::bound_assistant_text;
use crate::transform::review_reports::{
    FindingClass, Reports, findings_path, load_notes, load_reports, notes_path, render_reports,
};
use crate::transform::{Measurements, TransformArgs, resolve_harness, run_model_lane, write_evidence_json};

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
/// verdict line — the caller fails closed. Used only on harnesses that cannot
/// inject the report tools (muse / grok). Pure so the parse is testable
/// without spawning the critic.
fn parse_review_verdict(final_text: &str) -> Option<ReviewVerdict> {
    final_text.lines().rev().find_map(verdict_line)
}

fn verdict_line(line: &str) -> Option<ReviewVerdict> {
    match line.trim() {
        "VERDICT: pass" => Some(ReviewVerdict::Pass),
        "VERDICT: finding" => Some(ReviewVerdict::Finding),
        "VERDICT: environment" => Some(ReviewVerdict::Environment),
        _ => None,
    }
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
         see one and it is serious, `report_note` it as member-scope; do not `report_finding` it.\n\n\
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

/// Public assistant prose the shared stream-json derivation retained. Empty
/// and absent collapse to `None` so a content-less record stays presence-driven.
fn assistant_text(record: &serde_json::Value) -> Option<&str> {
    record.get("assistant_text").and_then(serde_json::Value::as_str).filter(|text| !text.is_empty())
}

/// The durable findings report: earlier public assistant text plus the terminal
/// result, with an identical terminal suffix kept once and the earlier prose
/// bounded, plus any schema-accepted `ReportFindings` payloads rendered beside
/// that prose (#5118). Verdict parsing does not read this string — only
/// [`final_text`].
fn compose_findings(record: &serde_json::Value) -> Option<String> {
    let prose = compose_prose(record);
    let structured = render_report_findings(record);
    match (prose, structured) {
        (None, None) => None,
        (Some(prose), None) => Some(prose),
        (None, Some(structured)) => Some(structured),
        (Some(prose), Some(structured)) => Some(insert_before_verdict(&prose, &structured)),
    }
}

/// The assistant-prose path: public assistant text plus the terminal result,
/// with an identical terminal suffix kept once. A review that never called
/// `ReportFindings` still composes exactly this.
fn compose_prose(record: &serde_json::Value) -> Option<String> {
    let assistant = assistant_text(record);
    let terminal = final_text(record).filter(|text| !text.is_empty());
    match (assistant, terminal) {
        (None, None) => None,
        (None, Some(terminal)) => Some(terminal.to_owned()),
        (Some(assistant), None) => Some(bound_assistant_text(assistant)),
        (Some(assistant), Some(terminal)) => Some(merge_assistant_and_terminal(assistant, terminal)),
    }
}

/// The last schema-accepted `ReportFindings` payload, rendered one finding per
/// line. Empty or absent is `None` so a prose-only review keeps the prose path.
fn render_report_findings(record: &serde_json::Value) -> Option<String> {
    let findings =
        record.get("report_findings").and_then(serde_json::Value::as_array).filter(|items| !items.is_empty())?;
    let rendered: Vec<String> = findings.iter().filter_map(render_reported_finding).collect();
    (!rendered.is_empty()).then(|| rendered.join("\n"))
}

/// One structured finding as a classified line. The summary stays first so a
/// `MECHANICAL` / `JUDGMENT` prefix the reviewer wrote there is still what
/// [`aether_bloomery::classify_findings`] reads (#4961 / #5118).
fn render_reported_finding(finding: &serde_json::Value) -> Option<String> {
    let summary =
        finding.get("summary").and_then(serde_json::Value::as_str).map(str::trim).filter(|text| !text.is_empty())?;
    let file = finding.get("file").and_then(serde_json::Value::as_str).unwrap_or("");
    let line = finding_line(finding.get("line"));
    let category = finding.get("category").and_then(serde_json::Value::as_str).unwrap_or("");
    let scenario = finding.get("failure_scenario").and_then(serde_json::Value::as_str).unwrap_or("");

    let location = match (file.is_empty(), line.as_deref()) {
        (true, None) => String::new(),
        (true, Some(line)) => line.to_owned(),
        (false, None) => file.to_owned(),
        (false, Some(line)) => format!("{file}:{line}"),
    };

    let mut rendered = format!("- {summary}");
    if location.is_empty() && category.is_empty() && scenario.is_empty() {
        return Some(rendered);
    }
    rendered.push_str(" — ");
    if !location.is_empty() {
        rendered.push_str(&location);
    }
    if !category.is_empty() {
        if !location.is_empty() {
            rendered.push(' ');
        }
        rendered.push('(');
        rendered.push_str(category);
        rendered.push(')');
    }
    if !scenario.is_empty() {
        if !location.is_empty() || !category.is_empty() {
            rendered.push_str(": ");
        }
        rendered.push_str(scenario);
    }
    Some(rendered)
}

fn finding_line(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(line) = value.as_u64() {
        return Some(line.to_string());
    }
    if let Some(line) = value.as_i64() {
        return Some(line.to_string());
    }
    value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
}

/// Slip `structured` in immediately before the last well-formed `VERDICT:` line
/// so the durable suffix stays the critic's own verdict. A report with no
/// verdict line just appends.
fn insert_before_verdict(prose: &str, structured: &str) -> String {
    last_verdict_line_start(prose).map_or_else(
        || format!("{prose}\n\n{structured}"),
        |at| {
            let head = prose[..at].trim_end();
            let tail = &prose[at..];
            if head.is_empty() {
                format!("{structured}\n\n{tail}")
            } else {
                format!("{head}\n\n{structured}\n\n{tail}")
            }
        },
    )
}

fn last_verdict_line_start(text: &str) -> Option<usize> {
    let mut found = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if verdict_line(line.trim_end_matches(['\n', '\r'])).is_some() {
            found = Some(offset);
        }
        offset += line.len();
    }
    found
}

fn merge_assistant_and_terminal(assistant: &str, terminal: &str) -> String {
    if assistant == terminal {
        return terminal.to_owned();
    }
    if let Some(prefix) = assistant.strip_suffix(terminal) {
        let prefix = prefix.trim_end();
        if prefix.is_empty() {
            return terminal.to_owned();
        }
        return format!("{}\n\n{terminal}", bound_assistant_text(prefix));
    }
    format!("{}\n\n{terminal}", bound_assistant_text(assistant))
}

/// Fold the review run's derived result record into the lane's terminal verdict
/// on harnesses without report tools: the critic's own `VERDICT:` line, but only
/// from a run that completed (`is_error == false` on the terminal result).
/// Everything else — a dead run, an errored run, a missing or malformed verdict
/// line — folds to `Finding`, fail-closed (a wrongly passed defect integrates; a
/// wrong finding just retries).
///
/// A `finding` verdict is then read by class (#4961): the critic states what
/// each finding is, and a report whose findings are all non-blocking advisories
/// folds to `Pass`. See [`advisory_only`] — the fail-closed direction is
/// preserved there too.
///
/// `Environment` survives the fold rather than collapsing into `Finding`
/// (ADR-0176): the critic judged no candidate, so the result is a claim about
/// the host, and flattening it here is what charged a member repair lap for an
/// executor outage. It is still gated on a clean completion — a run that died
/// cannot be trusted to have reported why. Pure so the gate is testable without
/// spawning Claude.
fn review_conclusion(record: &serde_json::Value) -> ReviewVerdict {
    if !completed_clean(record) {
        return ReviewVerdict::Finding;
    }
    match final_text(record).and_then(parse_review_verdict) {
        Some(ReviewVerdict::Finding) => advisory_only(record),
        Some(other) => other,
        None => ReviewVerdict::Finding,
    }
}

/// The terminal verdict a `finding` report earns once its findings are read by
/// class (#4961): `Pass` when the reviewer classified findings and marked none
/// of them blocking, `Finding` otherwise.
///
/// This is where "subjective findings must not break blooms" is actually
/// decided. The critic states a class per finding — mechanical, which asserts a
/// decidable property and blocks, or judgment, which is advisory unless the
/// critic marked it critical with a justification — and the lane derives what it
/// reports from those statements rather than from the bare existence of a
/// finding. The advisories still ride out in the stamped `findings` prose, so a
/// downgraded review records what it saw; it just stops pricing a taste call at
/// a repair round.
///
/// Fail-closed in the one direction that matters: a report with **no** classified
/// finding under a `finding` verdict stays a finding. That is the critic that
/// ignored the format, and reading unclassified prose as advisory would land
/// every defect a non-conforming reviewer names. The parse is the domain crate's
/// (`aether_bloomery::classify_findings`) rather than a local re-spelling, so the
/// lane that writes the format and the chassis that reads it cannot drift.
fn advisory_only(record: &serde_json::Value) -> ReviewVerdict {
    // Classification stays on the terminal result for a prose-only review, so
    // earlier narration cannot mint a pass (#5056). When the reviewer filed
    // through `ReportFindings`, those rendered lines are what
    // `classify_findings` is meant to read (#4961 / #5118).
    let terminal = final_text(record).unwrap_or_default();
    let report = render_report_findings(record)
        .map_or_else(|| terminal.to_owned(), |structured| format!("{structured}\n{terminal}"));
    let classified = aether_bloomery::classify_findings(&report);
    if classified.is_empty() || classified.any_blocking() {
        return ReviewVerdict::Finding;
    }

    ReviewVerdict::Pass
}

/// Whether the terminal result claims the run completed without a harness error.
fn completed_clean(record: &serde_json::Value) -> bool {
    record
        .get("result")
        .is_some_and(|result| result.get("is_error").and_then(serde_json::Value::as_bool) == Some(false))
}

/// Derive the Claude-path verdict from the findings file. A cleanly finished
/// run that reported nothing is a pass; a defect charges the candidate; only
/// environment reports mean the critic could not judge; a malformed file is a
/// lane shortfall (`environment`), never a candidate pass or fail. An errored
/// or dead run does not become a pass from an empty file — that path keeps the
/// existing lane-failure handling.
fn conclude_from_reports(record: &serde_json::Value, reports: &Reports) -> ReviewVerdict {
    match reports {
        Reports::Malformed { .. } => ReviewVerdict::Environment,
        Reports::Clean { findings } => {
            if findings.iter().any(|finding| finding.class == FindingClass::Defect) {
                return ReviewVerdict::Finding;
            }
            if !completed_clean(record) {
                return ReviewVerdict::Finding;
            }
            if findings.iter().any(|finding| finding.class == FindingClass::Environment) {
                return ReviewVerdict::Environment;
            }
            ReviewVerdict::Pass
        }
    }
}

/// Stamp evidence from the Claude findings file: the accumulated reports are
/// the findings text, never the terminal prose. Notes ride a separate key and
/// do not affect `status`.
fn stamp_reports_evidence(
    nonce: Option<&str>,
    record: &serde_json::Value,
    measured: Measurements,
    reports: &Reports,
    notes: Option<String>,
) -> serde_json::Value {
    let verdict = conclude_from_reports(record, reports);
    let findings = match reports {
        Reports::Malformed { reason } => Some(environment_findings(reason)),
        Reports::Clean { findings } => match verdict {
            ReviewVerdict::Pass => None,
            ReviewVerdict::Finding => Some(render_reports(findings)),
            ReviewVerdict::Environment => Some(environment_findings(&render_reports(findings))),
        },
    };
    let mut evidence = serde_json::json!({
        "command": REVIEW_CRITIC,
        "nonce": nonce,
        "status": status_token(verdict),
        "findings": findings,
        "result_record": record,
    });
    if let Some(notes) = notes {
        evidence["notes"] = serde_json::Value::String(notes);
    }
    measured.stamp(&mut evidence);
    evidence
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
    // Findings are the critic's public assistant text plus its terminal report
    // (#5056 / #3656) — stamped top-level so the local backend can persist them
    // and a later Refine re-entry is directed by what the critic actually found,
    // not a blind re-roll. The terminal `VERDICT:` line still decides
    // environment framing, so an earlier quoted host-fault cannot reclassify
    // a judged candidate. An `environment` report is framed as the host fault
    // it is, so the re-entry's reader is not handed a candidate defect that
    // was never found (#4723).
    let findings = compose_findings(record).map(|report| match final_text(record).and_then(parse_review_verdict) {
        Some(ReviewVerdict::Environment) => environment_findings(&report),
        _ => report,
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
/// critic headless, and fold its reports into the pass/fail `status` the local
/// backend's verdict derivation reads. Fail-closed at every shortfall. Like the
/// construct lane it needs a credential, so it runs worker-side — never on the
/// zero-secret path.
pub(super) fn run_review(args: &TransformArgs) -> Result<()> {
    // Pillars 3 and 5 judge the candidate against the repository's stated
    // conventions, so the critic is given the curated lane context rather than
    // told to go and read them (#4647, #5141) — a critic without the rules
    // cannot cite the one a candidate broke, and passes convention drift by
    // default.
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
        args.subject.as_deref(),
        args.task.as_deref(),
        None,
    );
    let run = run_model_lane(&prompt, args)?;
    // Claude injects the report tools and the findings file is the verdict
    // channel. Muse / grok have no injection and keep the text parse.
    let evidence = if matches!(resolve_harness(args.harness.as_deref())?, Harness::Claude) {
        stamp_reports_evidence(
            args.nonce.as_deref(),
            &run.record,
            run.measured,
            &load_reports(&findings_path(&args.out)),
            load_notes(&notes_path(&args.out)),
        )
    } else {
        stamp_review_evidence(args.nonce.as_deref(), review_conclusion(&run.record), &run.record, run.measured)
    };
    write_evidence_json(&args.out, &evidence)
}

#[cfg(test)]
mod tests {
    use super::{
        Measurements, ReviewVerdict, candidate_section, composition_contract, conclude_from_reports,
        parse_review_verdict, review_conclusion, stamp_reports_evidence, stamp_review_evidence,
    };
    use crate::transform::messages::{MAX_ASSISTANT_TEXT_BYTES, derive_result_record};
    use crate::transform::review_reports::{FindingClass, FindingReport, Reports};

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
    fn a_finding_verdict_reports_a_pass_when_every_finding_is_an_advisory() {
        use ReviewVerdict::{Finding, Pass};
        use serde_json::json;
        let clean = |text: &str| {
            derive_result_record(&format!("{}\n", json!({"type": "result", "is_error": false, "result": text})))
        };

        // Tripwire (#4961): the owner's requirement lives on this line —
        // subjective findings must not break blooms. A critic that classified
        // its findings and marked none of them blocking reports as a pass, so
        // the composition never re-weaves over a naming preference; the prose
        // still rides out for the record.
        assert_eq!(
            review_conclusion(&clean(
                "- JUDGMENT — src/reduce.rs: `weave` would read better as `composition`.\n\
                 - JUDGMENT — src/lib.rs: the module doc buries its rule.\n\
                 VERDICT: finding"
            )),
            Pass,
        );
        // A judgment call the critic marked critical *and* justified blocks
        // exactly as it always did.
        assert_eq!(
            review_conclusion(&clean(
                "- JUDGMENT (critical: the seam drops the retry budget, so a wedge lands silently) — src/reduce.rs\n\
                 VERDICT: finding"
            )),
            Finding,
        );
        // The marker without the sentence is not the mark. Its own tripwire:
        // reading a bare `(critical)` as blocking hands every taste call a
        // one-word escalation and re-opens the door this change closes.
        assert_eq!(
            review_conclusion(&clean("- JUDGMENT (critical) — src/reduce.rs: I feel strongly.\nVERDICT: finding")),
            Pass,
        );
        // Mechanical blocks, with or without its named check — a decidable
        // defect is a defect, and a formatting slip must never land one.
        assert_eq!(
            review_conclusion(&clean(
                "- MECHANICAL (check: `representative_covers_every_decision`) — the fixture omits it.\n\
                 VERDICT: finding"
            )),
            Finding,
        );
        assert_eq!(
            review_conclusion(&clean("- MECHANICAL — src/lib.rs: the guard is unexercised.\nVERDICT: finding")),
            Finding,
        );
        // And the fail-closed floor: a `finding` verdict whose prose classifies
        // nothing is a critic that ignored the format, not a set of advisories.
        assert_eq!(
            review_conclusion(&clean("src/lib.rs: an empty list makes this index panic.\nVERDICT: finding")),
            Finding,
            "an unclassified finding is read as blocking",
        );
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

    fn transcript(events: &[serde_json::Value]) -> String {
        events.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
    }

    fn assistant(content: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "model": "claude-opus-4-8",
                "content": content,
                "usage": {"input_tokens": 1},
            },
        })
    }

    fn text(body: &str) -> serde_json::Value {
        serde_json::json!([{"type": "text", "text": body}])
    }

    fn result(body: &str) -> serde_json::Value {
        serde_json::json!({"type": "result", "is_error": false, "result": body})
    }

    fn stamped(record: &serde_json::Value) -> serde_json::Value {
        stamp_review_evidence(Some("n-5056"), review_conclusion(record), record, Measurements::default())
    }

    #[test]
    fn findings_keep_assistant_prose_that_never_reached_the_terminal_result() {
        // Tripwire (#5056): Wave 1 recorded only `VERDICT: finding` while the
        // actionable report stayed in earlier assistant text. Tool payloads in
        // the same transcript must not become repair instructions.
        let record = derive_result_record(&transcript(&[
            assistant(&serde_json::json!([
                {"type": "text", "text": "- MECHANICAL — src/lib.rs: empty input panics."},
                {"type": "tool_use", "name": "Read", "input": {"path": "TOOL_INPUT_SECRET"}},
            ])),
            serde_json::json!({
                "type": "user",
                "message": {"content": [
                    {"type": "tool_result", "content": "TOOL_RESULT_BODY"},
                ]},
            }),
            assistant(&text("The guard is missing on the empty-list path.")),
            result("VERDICT: finding"),
        ]));

        let evidence = stamped(&record);
        let findings = evidence["findings"].as_str().expect("a completed finding run stamps findings");
        assert!(findings.contains("empty input panics"), "the first assistant block survives: {findings}");
        assert!(findings.contains("empty-list path"), "a later assistant block survives: {findings}");
        assert!(findings.contains("VERDICT: finding"), "the terminal verdict stays in the report: {findings}");
        assert_eq!(findings.matches("VERDICT: finding").count(), 1, "the terminal line is not duplicated: {findings}");
        assert!(!findings.contains("TOOL_INPUT_SECRET"), "tool-use input is not findings: {findings}");
        assert!(!findings.contains("TOOL_RESULT_BODY"), "tool results are not findings: {findings}");
        assert_eq!(evidence["status"], "fail");
        assert_eq!(review_conclusion(&record), ReviewVerdict::Finding);
    }

    #[test]
    fn duplicated_terminal_text_is_emitted_once() {
        // Tripwire: the last assistant message is typically copied onto the
        // terminal `result`. Stamping both would double the report a Refine reads.
        let report = "- MECHANICAL — src/lib.rs: empty input panics.\nVERDICT: finding";
        let record = derive_result_record(&transcript(&[assistant(&text(report)), result(report)]));

        assert_eq!(
            stamped(&record)["findings"].as_str(),
            Some(report),
            "identical terminal text is not appended again",
        );
    }

    #[test]
    fn an_earlier_quoted_verdict_cannot_shadow_the_terminal_verdict() {
        // Tripwire: concatenating assistant text into the verdict parse would
        // let a quoted or superseded `VERDICT:` decide the outcome.
        let record = derive_result_record(&transcript(&[
            assistant(&text("the instructions say to end with\nVERDICT: pass")),
            result("src/lib.rs: the index panics on empty input.\nVERDICT: finding"),
        ]));

        assert_eq!(review_conclusion(&record), ReviewVerdict::Finding);
        let evidence = stamped(&record);
        let findings = evidence["findings"].as_str().expect("findings");
        assert!(findings.contains("VERDICT: pass"), "the quoted earlier line is still in the report: {findings}");
        assert!(findings.contains("the index panics"), "the terminal report is kept: {findings}");
        assert!(findings.ends_with("VERDICT: finding"), "the terminal verdict is the durable suffix: {findings}");

        let flipped = derive_result_record(&transcript(&[
            assistant(&text("earlier I wrote\nVERDICT: finding")),
            result("on reflection every pillar is clean.\nVERDICT: pass"),
        ]));
        assert_eq!(review_conclusion(&flipped), ReviewVerdict::Pass);
        assert_eq!(stamped(&flipped)["status"], "pass");
    }

    #[test]
    fn retained_prose_cannot_synthesize_a_pass_or_drop_the_terminal_verdict() {
        // Tripwire: advisory-only classification stays on the terminal result.
        // Earlier JUDGMENT lines must not downgrade a verdict-only `finding`
        // into a pass, and a dead run with leftover prose still fails closed.
        let classified_early = derive_result_record(&transcript(&[
            assistant(&text("- JUDGMENT — src/lib.rs: the name is ugly.")),
            result("VERDICT: finding"),
        ]));
        assert_eq!(
            review_conclusion(&classified_early),
            ReviewVerdict::Finding,
            "retained advisory prose must not mint a pass",
        );
        assert!(
            stamped(&classified_early)["findings"].as_str().expect("findings").contains("the name is ugly"),
            "the advisory still rides the findings channel",
        );

        let died = derive_result_record(&transcript(&[assistant(&text("src/lib.rs: empty input panics."))]));
        assert_eq!(review_conclusion(&died), ReviewVerdict::Finding);
        assert_eq!(stamped(&died)["status"], "fail");
        assert_eq!(
            stamped(&died)["findings"].as_str(),
            Some("src/lib.rs: empty input panics."),
            "a dead run still retains the prose it produced",
        );
    }

    #[test]
    fn environment_framing_still_wraps_the_composed_report() {
        let record = derive_result_record(&transcript(&[
            assistant(&text("git diff failed: bwrap: loopback failed.")),
            result("VERDICT: environment"),
        ]));

        assert_eq!(review_conclusion(&record), ReviewVerdict::Environment);
        let evidence = stamped(&record);
        let findings = evidence["findings"].as_str().expect("environment findings");
        assert!(findings.contains("bwrap: loopback failed"), "earlier host-fault prose survives: {findings}");
        assert!(findings.contains("VERDICT: environment"), "the terminal token stays in the report: {findings}");
        assert!(
            findings.contains("not the same as the candidate failing"),
            "the operator framing still wraps the report: {findings}",
        );
    }

    #[test]
    fn a_bounded_prefix_never_drops_the_terminal_verdict() {
        // Tripwire: bounding the retained prose must not eat the terminal
        // `VERDICT:` line — that token is what operators and adjudication read.
        let prefix = "x".repeat(MAX_ASSISTANT_TEXT_BYTES + 64);
        let record = derive_result_record(&transcript(&[
            assistant(&text(&prefix)),
            result("src/lib.rs: empty input panics.\nVERDICT: finding"),
        ]));
        let evidence = stamped(&record);
        let findings = evidence["findings"].as_str().expect("findings");
        assert!(findings.len() <= MAX_ASSISTANT_TEXT_BYTES + 80, "the prefix is bounded: {}", findings.len());
        assert!(findings.ends_with("VERDICT: finding"), "the terminal verdict survives the bound: {findings}");
        assert!(findings.contains("empty input panics"), "the terminal report body survives: {findings}");
    }

    fn report_findings_use(id: &str, findings: &serde_json::Value) -> serde_json::Value {
        assistant(&serde_json::json!([{
            "type": "tool_use",
            "id": id,
            "name": "ReportFindings",
            "input": {"findings": findings},
        }]))
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> serde_json::Value {
        let mut block = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
        });
        if is_error {
            block["is_error"] = serde_json::json!(true);
        }
        serde_json::json!({"type": "user", "message": {"content": [block]}})
    }

    #[test]
    fn report_findings_are_frozen_from_the_last_accepted_tool_call() {
        // Tripwire (#5118): Wave 1 and bloom 4604e4a5 froze "see finding below"
        // plus VERDICT while the actual findings sat in ReportFindings. A
        // rejected first call (schema validation) must not win, a later accepted
        // call must, and every field the payload carries has to reach the
        // durable report so a repair / delta-confirm can read it.
        let rejected = serde_json::json!([{
            "file": "src/wrong.rs",
            "line": 1,
            "category": "correctness",
            "summary": "MECHANICAL — rejected payload must not freeze",
            "failure_scenario": "this call was schema-rejected",
        }]);
        let accepted = serde_json::json!([
            {
                "file": "scripts/bloomery-operator.py",
                "line": 65,
                "category": "correctness",
                "summary": "MECHANICAL (check: `cmd_flakes_decodes_machinery_names`) — FACT_NAMES never grew the #5091 tail",
                "failure_scenario": "a live journal's machinery facts render as unknown(28)",
            },
            {
                "file": "src/intake/tests.rs",
                "line": 855,
                "category": "test-integrity",
                "summary": "JUDGMENT — three comments still say intake refuses ExecutorFault",
                "failure_scenario": "a reader of those comments is told the opposite of current admission",
            },
        ]);
        let record = derive_result_record(&transcript(&[
            assistant(&text("5092 is not: see finding below.")),
            report_findings_use("t-reject", &rejected),
            tool_result(
                "t-reject",
                "<tool_use_error>InputValidationError: short_summary too long</tool_use_error>",
                true,
            ),
            report_findings_use("t-accept", &accepted),
            tool_result("t-accept", "2 findings reported.", false),
            assistant(&serde_json::json!([
                {"type": "tool_use", "name": "Read", "input": {"path": "TOOL_INPUT_SECRET"}},
            ])),
            result("VERDICT: finding"),
        ]));

        let evidence = stamped(&record);
        let findings = evidence["findings"].as_str().expect("a completed finding run stamps findings");
        assert!(findings.contains("5092 is not: see finding below"), "the assistant prose survives: {findings}");
        assert!(
            findings.contains("FACT_NAMES never grew the #5091 tail"),
            "the accepted mechanical finding is frozen: {findings}",
        );
        assert!(
            findings.contains("three comments still say intake refuses ExecutorFault"),
            "the accepted judgment finding is frozen: {findings}",
        );
        assert!(
            findings.contains(
                "scripts/bloomery-operator.py:65 (correctness): a live journal's machinery facts render as unknown(28)"
            ),
            "file, line, category, and failure scenario ride the mechanical line: {findings}",
        );
        assert!(
            findings.contains("src/intake/tests.rs:855 (test-integrity): a reader of those comments is told the opposite of current admission"),
            "file, line, category, and failure scenario ride the judgment line: {findings}",
        );
        assert!(
            !findings.contains("rejected payload must not freeze"),
            "a schema-rejected call is ignored: {findings}"
        );
        assert!(!findings.contains("TOOL_INPUT_SECRET"), "non-findings tool input is still not findings: {findings}");
        assert!(findings.ends_with("VERDICT: finding"), "the terminal verdict stays the durable suffix: {findings}");
        assert_eq!(findings.matches("VERDICT: finding").count(), 1, "the terminal line is not duplicated: {findings}");

        let classified = aether_bloomery::classify_findings(findings);
        assert_eq!(classified.findings.len(), 2, "{classified:?}");
        assert!(classified.any_blocking(), "the mechanical prefix must still classify as blocking: {classified:?}");
        assert_eq!(
            classified.advisories().count(),
            1,
            "the judgment prefix must still classify as advisory: {classified:?}"
        );
        assert_eq!(
            classified.named_checks().collect::<Vec<_>>(),
            ["`cmd_flakes_decodes_machinery_names`"],
            "the check named inside the summary survives rendering: {classified:?}",
        );
        assert_eq!(review_conclusion(&record), ReviewVerdict::Finding);
        assert_eq!(evidence["status"], "fail");
    }

    #[test]
    fn a_tool_only_advisory_report_still_classifies_as_a_pass() {
        // Tripwire (#5118 / #4961): a reviewer that follows the harness puts
        // JUDGMENT only in ReportFindings and ends with VERDICT: finding. The
        // prefixes have to survive so the lane reads them the same way it reads
        // prose; otherwise every harness-following advisory review blocks.
        let record = derive_result_record(&transcript(&[
            assistant(&text("see finding below.")),
            report_findings_use(
                "t-1",
                &serde_json::json!([{
                    "file": "src/lib.rs",
                    "line": 4,
                    "category": "economy",
                    "summary": "JUDGMENT — `weave` would read better as `composition`",
                    "failure_scenario": "a later reader spends a moment on the name",
                }]),
            ),
            tool_result("t-1", "1 finding reported.", false),
            result("VERDICT: finding"),
        ]));

        assert_eq!(review_conclusion(&record), ReviewVerdict::Pass);
        let evidence = stamped(&record);
        assert_eq!(evidence["status"], "pass");
        let findings = evidence["findings"].as_str().expect("findings");
        assert!(findings.contains("JUDGMENT — `weave` would read better as `composition`"), "{findings}");
        assert!(findings.contains("VERDICT: finding"), "{findings}");
    }

    #[test]
    fn a_prose_only_review_is_unchanged_when_the_transcript_has_no_report_findings() {
        // Tripwire (#5118): no tool call means today's path. An unmatched
        // ReportFindings (no tool_result) is not schema-accepted, so it must
        // not invent findings the reviewer never landed.
        let report = "- MECHANICAL — src/lib.rs: empty input panics.\nVERDICT: finding";
        let record = derive_result_record(&transcript(&[assistant(&text(report)), result(report)]));
        assert_eq!(stamped(&record)["findings"].as_str(), Some(report));

        let unmatched = derive_result_record(&transcript(&[
            assistant(&text("see finding below.")),
            report_findings_use(
                "t-pending",
                &serde_json::json!([{
                    "file": "src/lib.rs",
                    "summary": "MECHANICAL — never accepted, must not freeze",
                }]),
            ),
            result("VERDICT: finding"),
        ]));
        let unmatched_evidence = stamped(&unmatched);
        let findings = unmatched_evidence["findings"].as_str().expect("findings");
        assert_eq!(findings, "see finding below.\n\nVERDICT: finding");
        assert!(!findings.contains("never accepted"), "an unmatched call is not accepted: {findings}");
    }

    fn clean_record(text: &str) -> serde_json::Value {
        derive_result_record(&format!("{}\n", serde_json::json!({"type": "result", "is_error": false, "result": text})))
    }

    fn errored_record(text: &str) -> serde_json::Value {
        derive_result_record(&format!("{}\n", serde_json::json!({"type": "result", "is_error": true, "result": text})))
    }

    fn defect(summary: &str, detail: &str) -> FindingReport {
        FindingReport { summary: summary.to_owned(), detail: detail.to_owned(), class: FindingClass::Defect }
    }

    fn environment(summary: &str, detail: &str) -> FindingReport {
        FindingReport { summary: summary.to_owned(), detail: detail.to_owned(), class: FindingClass::Environment }
    }

    // Pre-fix a clean Claude run with no VERDICT: line stamped fail and handed
    // its pass prose to Refine as findings. An empty reports file is a pass.
    #[test]
    fn a_clean_claude_run_with_no_reports_passes_without_a_verdict_line() {
        let record = clean_record("all five pillars hold; no defect I can name.");
        let reports = Reports::Clean { findings: Vec::new() };
        assert_eq!(conclude_from_reports(&record, &reports), ReviewVerdict::Pass);
        let evidence = stamp_reports_evidence(Some("n-pass"), &record, Measurements::default(), &reports, None);
        assert_eq!(evidence["status"], "pass");
        assert!(evidence["findings"].is_null(), "a clean pass does not stamp terminal prose as findings");
    }

    // Pre-fix the fail path persisted the terminal prose. The stamped findings
    // must be the accumulated reports, in order.
    #[test]
    fn a_defect_report_fails_and_stamps_the_reports_not_the_terminal_prose() {
        let record = clean_record("looks fine to me, shipping it.");
        let reports = Reports::Clean {
            findings: vec![defect("empty input panics", "src/lib.rs: unguarded index on an empty list")],
        };
        assert_eq!(conclude_from_reports(&record, &reports), ReviewVerdict::Finding);
        let evidence = stamp_reports_evidence(None, &record, Measurements::default(), &reports, None);
        assert_eq!(evidence["status"], "fail");
        let findings = evidence["findings"].as_str().expect("findings");
        assert!(findings.contains("empty input panics"), "{findings}");
        assert!(findings.contains("unguarded index"), "{findings}");
        assert!(!findings.contains("looks fine to me"), "terminal prose is not the findings channel: {findings}");
    }

    #[test]
    fn only_environment_reports_stamp_environment() {
        let record = clean_record("could not run git diff");
        let reports = Reports::Clean { findings: vec![environment("git diff failed", "bwrap: loopback failed")] };
        assert_eq!(conclude_from_reports(&record, &reports), ReviewVerdict::Environment);
        let evidence = stamp_reports_evidence(None, &record, Measurements::default(), &reports, None);
        assert_eq!(evidence["status"], "environment");
        let findings = evidence["findings"].as_str().expect("findings");
        assert!(findings.contains("bwrap: loopback failed"), "{findings}");
        assert!(findings.contains("not the same as the candidate failing"), "{findings}");
    }

    #[test]
    fn an_errored_run_with_an_empty_findings_file_does_not_pass() {
        let record = errored_record("");
        let reports = Reports::Clean { findings: Vec::new() };
        assert_ne!(conclude_from_reports(&record, &reports), ReviewVerdict::Pass);
        assert_eq!(stamp_reports_evidence(None, &record, Measurements::default(), &reports, None)["status"], "fail",);
    }

    #[test]
    fn a_malformed_findings_file_is_environment_never_pass_or_fail() {
        let record = clean_record("VERDICT: pass");
        let reports = Reports::Malformed { reason: "line 1: truncated line".to_owned() };
        assert_eq!(conclude_from_reports(&record, &reports), ReviewVerdict::Environment);
        let evidence = stamp_reports_evidence(None, &record, Measurements::default(), &reports, None);
        assert_eq!(evidence["status"], "environment");
        assert_ne!(evidence["status"], "pass");
        assert_ne!(evidence["status"], "fail");
    }
}
