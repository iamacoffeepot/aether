//! The `retrospect.read` lane (ADR-0216): assemble the reader prompt from the
//! ADR-0214 bundle's static instruction fields plus the bloom id, receipt
//! digest, and landed range as context sections, run the resolved harness over
//! the checked-out landed head, and stamp the `retrospect_findings` array the
//! local backend already parses.
//!
//! The lane reads the range as evidence and never writes to the tree. A
//! well-formed reply yields the array as claimed; a reply with no parseable
//! array yields an empty one and a note in `findings` prose. Entries are not
//! repaired — the intake refuses a malformed emission whole.

use aether_bloomery::RetrospectClaim;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::bloom::{RETROSPECT, RETROSPECT_FINDING_CONTRACT};
use crate::transform::conventions;
use crate::transform::lane::Resumed;
use crate::transform::{Measurements, TransformArgs, run_model_lane, write_evidence_json};

/// The typed id of the bloom-level reader lane. Recognized here so an unknown
/// id stays unmapped exactly as in the other lanes.
pub(super) use aether_bloomery::RETROSPECT_READ_COMMAND as RETROSPECT_READ;

/// The status the local backend already knows how to read. A completed read —
/// including one that files nothing — is a pass: findings are a product, never
/// a gate. A run that never judged stamps `environment` so the executor raises
/// a host fault rather than a failing study.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RetrospectStatus {
    Pass,
    Environment,
}

fn status_token(status: RetrospectStatus) -> &'static str {
    match status {
        RetrospectStatus::Pass => "pass",
        RetrospectStatus::Environment => "environment",
    }
}

/// The findings prose a completed reply with no parseable array produces.
const UNPARSEABLE_NOTE: &str = "the reader reply carried no parseable retrospect_findings array; filing nothing";

/// The findings prose a run that never reached a terminal result produces.
const INCOMPLETE_NOTE: &str = "the reader did not complete; filing nothing";

/// Assemble the reader prompt from the sealed instruction fields plus the
/// context slots ADR-0216 names. The bloom id, receipt digest, and landed range
/// sit in `##` sections after the static text; they are never interpolated into
/// it.
fn assemble_retrospect_prompt(
    bloom: Option<&str>,
    receipt: Option<&str>,
    diff_base: Option<&str>,
    subject: Option<&str>,
) -> String {
    let conventions_section = format!("{}\n\n", conventions::section());
    format!(
        "{conventions_section}{RETROSPECT}\n\n{RETROSPECT_FINDING_CONTRACT}\n{}{}{}",
        context_section("Bloom", bloom),
        context_section("Receipt digest", receipt),
        landed_range_section(diff_base, subject),
    )
}

fn context_section(heading: &str, value: Option<&str>) -> String {
    value.map_or_else(String::new, |value| format!("\n## {heading}\n\n{value}\n"))
}

fn landed_range_section(diff_base: Option<&str>, subject: Option<&str>) -> String {
    match (diff_base, subject) {
        (Some(base), Some(head)) => format!("\n## Landed range\n\n`{base}..{head}`\n"),
        (Some(base), None) => format!("\n## Landed range\n\n`{base}..HEAD`\n"),
        (None, Some(head)) => format!("\n## Landed range\n\n`..{head}`\n"),
        (None, None) => String::new(),
    }
}

/// The critic's final message text, if the run reached one.
fn final_text(record: &Value) -> Option<&str> {
    record.get("result").and_then(|result| result.get("result")).and_then(Value::as_str)
}

fn assistant_text(record: &Value) -> Option<&str> {
    record.get("assistant_text").and_then(Value::as_str).filter(|text| !text.is_empty())
}

fn completed_clean(record: &Value) -> bool {
    record.get("result").is_some_and(|result| result.get("is_error").and_then(Value::as_bool) == Some(false))
}

/// Parse the reader's claimed findings from its reply. A well-formed JSON array
/// — or an object carrying a `retrospect_findings` array — yields the claims as
/// written, including empty titles and surfaces; nothing is repaired. `None`
/// when the reply carries no parseable array at all.
fn parse_retrospect_claims(text: &str) -> Option<Vec<RetrospectClaim>> {
    let trimmed = text.trim();
    if let Some(claims) = claims_from_json_prefix(trimmed) {
        return Some(claims);
    }
    for block in fenced_blocks(text) {
        if let Some(claims) = claims_from_json_prefix(block.trim()) {
            return Some(claims);
        }
    }
    last_json_value_claims(text)
}

fn claims_from_json_prefix(text: &str) -> Option<Vec<RetrospectClaim>> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = Value::deserialize(&mut deserializer).ok()?;
    claims_from_value(&value)
}

fn claims_from_value(value: &Value) -> Option<Vec<RetrospectClaim>> {
    let entries = match value {
        Value::Array(entries) => entries.as_slice(),
        Value::Object(_) => value.get("retrospect_findings")?.as_array()?.as_slice(),
        _ => return None,
    };
    Some(entries.iter().map(claim_from_entry).collect())
}

fn claim_from_entry(entry: &Value) -> RetrospectClaim {
    RetrospectClaim {
        title: entry.get("title").and_then(Value::as_str).unwrap_or_default().to_owned(),
        body: entry.get("body").and_then(Value::as_str).unwrap_or_default().to_owned(),
        surface: entry
            .get("surface")
            .and_then(Value::as_array)
            .map(|globs| globs.iter().map(|glob| glob.as_str().unwrap_or_default().to_owned()).collect())
            .unwrap_or_default(),
    }
}

fn fenced_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = rest[start + 3..].strip_prefix("json").unwrap_or_else(|| &rest[start + 3..]);
        let after = after.strip_prefix('\n').or_else(|| after.strip_prefix('\r')).unwrap_or(after);
        let Some(end) = after.find("```") else {
            break;
        };
        blocks.push(&after[..end]);
        rest = &after[end + 3..];
    }
    blocks
}

fn last_json_value_claims(text: &str) -> Option<Vec<RetrospectClaim>> {
    let mut found = None;
    for (index, byte) in text.bytes().enumerate() {
        if (byte == b'[' || byte == b'{')
            && let Some(claims) = claims_from_json_prefix(&text[index..])
        {
            found = Some(claims);
        }
    }
    found
}

fn claims_from_record(record: &Value) -> Option<Vec<RetrospectClaim>> {
    if let Some(text) = final_text(record)
        && let Some(claims) = parse_retrospect_claims(text)
    {
        return Some(claims);
    }
    assistant_text(record).and_then(parse_retrospect_claims)
}

/// Stamp the broker-matched `nonce`, the parsed claims, and the result record
/// onto the reader's evidence envelope. An empty array is a passing read with
/// nothing to file. Pure so the binding is testable without running a harness.
fn stamp_retrospect_evidence(nonce: Option<&str>, record: &Value, measured: Measurements) -> Value {
    let (status, findings, note) = if completed_clean(record) {
        let (findings, note) = claims_from_record(record)
            .map_or_else(|| (Vec::new(), Some(UNPARSEABLE_NOTE)), |findings| (findings, None));
        (RetrospectStatus::Pass, findings, note)
    } else {
        (RetrospectStatus::Environment, Vec::new(), Some(INCOMPLETE_NOTE))
    };
    let mut evidence = serde_json::json!({
        "command": RETROSPECT_READ,
        "nonce": nonce,
        "status": status_token(status),
        "retrospect_findings": findings,
        "result_record": record,
    });
    if let Some(note) = note
        && let Some(object) = evidence.as_object_mut()
    {
        object.insert("findings".to_owned(), Value::String(note.to_owned()));
    }
    measured.stamp(&mut evidence);
    evidence
}

/// The `retrospect.read` lane: assemble the prompt from the bundle fields and
/// the order's context slots, run the resolved harness, and stamp evidence.
/// Like the other model lanes it needs a credential, so it runs worker-side.
pub(super) fn run_retrospect(args: &TransformArgs) -> Result<()> {
    let prompt = assemble_retrospect_prompt(
        args.bloom.as_deref(),
        args.receipt.as_deref(),
        args.diff_base.as_deref(),
        args.subject.as_deref(),
    );
    let run = run_model_lane(&prompt, args, Resumed::AfterReset)?;
    write_evidence_json(&args.out, &stamp_retrospect_evidence(args.nonce.as_deref(), &run.record, run.measured))
}

#[cfg(test)]
mod tests {
    use super::{
        INCOMPLETE_NOTE, RETROSPECT, RETROSPECT_FINDING_CONTRACT, UNPARSEABLE_NOTE, assemble_retrospect_prompt,
        parse_retrospect_claims, stamp_retrospect_evidence,
    };
    use crate::transform::Measurements;
    use crate::transform::messages::derive_result_record;
    use serde_json::json;

    fn record(is_error: bool, text: &str) -> serde_json::Value {
        derive_result_record(&format!("{}\n", json!({"type": "result", "is_error": is_error, "result": text})))
    }

    fn two_findings_json() -> String {
        json!([
            {
                "title": "the drain re-reads a parked entry every tick",
                "body": "The study drain acks past a parked entry, so the next tick re-selects it.",
                "surface": ["crates/aether-chassis-bloomery/**"],
            },
            {
                "title": "a refused emission logs no bloom id",
                "body": "The refusal warn names the nonce and not the bloom, so it cannot be traced back.",
                "surface": ["crates/aether-bloomery/**"],
            },
        ])
        .to_string()
    }

    #[test]
    fn a_model_reply_carrying_two_findings_stamps_them_and_a_result_record() {
        // Tripwire: the local backend reads the top-level `retrospect_findings`
        // array as `RetrospectClaim` (title, body, surface) and the result
        // record for study. A lane that nested the array, repaired entries, or
        // dropped the record would hand intake a shape it never sees.
        let evidence =
            stamp_retrospect_evidence(Some("n-read"), &record(false, &two_findings_json()), Measurements::default());

        assert_eq!(evidence["command"], "retrospect.read");
        assert_eq!(evidence["nonce"], "n-read");
        assert_eq!(evidence["status"], "pass");
        assert!(evidence.get("result_record").is_some(), "the result record rides the envelope");
        let findings = evidence["retrospect_findings"].as_array().expect("the channel is an array");
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert_eq!(findings[0]["title"], "the drain re-reads a parked entry every tick");
        assert_eq!(findings[0]["surface"], json!(["crates/aether-chassis-bloomery/**"]));
        assert_eq!(findings[1]["title"], "a refused emission logs no bloom id");
        assert_eq!(findings[1]["surface"], json!(["crates/aether-bloomery/**"]));
        assert!(evidence.get("findings").is_none(), "a well-formed reply stamps no findings note");
    }

    #[test]
    fn a_reply_with_no_parseable_array_files_nothing_and_notes_why() {
        let evidence = stamp_retrospect_evidence(
            None,
            &record(false, "the bloom left a seam I cannot name as JSON"),
            Measurements::default(),
        );

        assert_eq!(evidence["status"], "pass", "findings are a product, never a gate");
        assert_eq!(evidence["retrospect_findings"], json!([]));
        assert_eq!(evidence["findings"], UNPARSEABLE_NOTE);
    }

    #[test]
    fn an_incomplete_run_is_environment_and_files_nothing() {
        let evidence =
            stamp_retrospect_evidence(None, &record(true, two_findings_json().as_str()), Measurements::default());

        assert_eq!(evidence["status"], "environment");
        assert_eq!(evidence["retrospect_findings"], json!([]));
        assert_eq!(evidence["findings"], INCOMPLETE_NOTE);
    }

    #[test]
    fn malformed_entries_are_stamped_as_claimed_not_repaired() {
        // Tripwire: the intake refuses a malformed emission whole. Dropping or
        // filling in a missing title here would hide the broken entry from that
        // refusal and file the survivors.
        let reply = json!([
            {"title": "a real finding", "body": "with a body and a surface", "surface": ["crates/aether-bloomery/**"]},
            {"title": "", "body": "and a sibling with no title at all", "surface": ["crates/aether-bloomery/**"]},
        ])
        .to_string();
        let evidence = stamp_retrospect_evidence(None, &record(false, &reply), Measurements::default());
        let findings = evidence["retrospect_findings"].as_array().expect("array");

        assert_eq!(findings.len(), 2);
        assert_eq!(findings[1]["title"], "");
        assert_eq!(findings[1]["body"], "and a sibling with no title at all");
    }

    #[test]
    fn the_prompt_names_context_in_sections_never_inside_the_instructions() {
        // Tripwire (ADR-0216 §2): the bloom id, receipt digest, and landed range
        // are prompt-manifest context slots. Interpolating them into the sealed
        // instruction text is the thing ADR-0214 forbids.
        let bloom = "bloom-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let receipt = "receipt-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let base = "base-cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        let head = "head-dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let prompt = assemble_retrospect_prompt(Some(bloom), Some(receipt), Some(base), Some(head));

        assert!(prompt.contains(RETROSPECT), "the process instructions are present");
        assert!(prompt.contains(RETROSPECT_FINDING_CONTRACT), "the finding contract is present");
        assert!(prompt.contains("## Bloom"), "{prompt}");
        assert!(prompt.contains("## Receipt digest"), "{prompt}");
        assert!(prompt.contains("## Landed range"), "{prompt}");
        assert!(prompt.contains(&format!("`{base}..{head}`")), "the landed range is the git range: {prompt}");
        let instruction_end = prompt.find("## Bloom").expect("the bloom section follows the instructions");
        assert!(!prompt[..instruction_end].contains(bloom), "the bloom id is not interpolated into the instructions");
        assert!(
            !prompt[..instruction_end].contains(receipt),
            "the receipt digest is not interpolated into the instructions"
        );
        assert!(!prompt[..instruction_end].contains(base), "the sealed base is not interpolated into the instructions");
        assert!(!prompt[..instruction_end].contains(head), "the landed head is not interpolated into the instructions");
    }

    #[test]
    fn an_object_envelope_and_a_fenced_array_both_parse() {
        let wrapped = json!({
            "retrospect_findings": [{"title": "one", "body": "body", "surface": ["xtask/**"]}]
        })
        .to_string();
        let from_object = parse_retrospect_claims(&wrapped).expect("object envelope");
        assert_eq!(from_object.len(), 1);
        assert_eq!(from_object[0].title, "one");

        let fenced = format!("here they are\n```json\n{}\n```\n", two_findings_json());
        let from_fence = parse_retrospect_claims(&fenced).expect("fenced array");
        assert_eq!(from_fence.len(), 2);
        assert_eq!(from_fence[0].title, "the drain re-reads a parked entry every tick");
    }
}
