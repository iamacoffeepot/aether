//! Reader meanings for journal records: one template per (fact, outcome) pair.
//!
//! The journal list paints fixed columns plus this meaning, and search matches
//! the rendered line including it, so `refused` or `retired` finds events by
//! what they mean rather than by the coordinator's vocabulary. Unknown pairs
//! fall back to the outcome name so a new fact never blanks the line. Adding
//! a fact is one row in [`meaning`].

use serde_json::Value;

use crate::dto::JournalRecordView;
use crate::palette::Role;

/// Fixed column widths for the journal list, in characters.
pub const SEQ_WIDTH: usize = 5;
/// Recorded time as `HH:MM:SS`.
pub const TIME_WIDTH: usize = 8;
/// Fact variant name.
pub const FACT_WIDTH: usize = 30;
/// Bloom digest prefix.
pub const BLOOM_WIDTH: usize = 8;
/// Member workpiece.
pub const MEMBER_WIDTH: usize = 24;
/// Outcome variant name.
pub const OUTCOME_WIDTH: usize = 23;

/// The fact variant this record journaled, or the raw event when unshaped.
#[must_use]
pub fn fact_name(record: &JournalRecordView) -> String {
    variant_name(&record.event, "fact")
}

/// The outcome the event reduced to, or the raw outcome when unshaped.
#[must_use]
pub fn outcome_name(record: &JournalRecordView) -> String {
    variant_name(&record.outcome, "outcome")
}

/// Eight-character bloom prefix for the bloom column, blank when absent.
#[must_use]
pub fn bloom_prefix(record: &JournalRecordView) -> String {
    fact_bits(&record.event).bloom.unwrap_or_default()
}

/// Member workpiece for the member column, blank when absent.
#[must_use]
pub fn member_name(record: &JournalRecordView) -> String {
    fact_bits(&record.event).member.unwrap_or_default()
}

/// Recorded time as `HH:MM:SS` in UTC, blank when the row predates the stamp.
#[must_use]
pub fn recorded_time(record: &JournalRecordView) -> String {
    let Some(millis) = record.recorded_unix_millis else {
        return String::new();
    };
    let secs = millis / 1_000;
    let day_secs = secs % 86_400;
    let hour = day_secs / 3_600;
    let minute = (day_secs % 3_600) / 60;
    let second = day_secs % 60;
    format!("{hour:02}:{minute:02}:{second:02}")
}

/// Paint role for a fact class: lifecycle facts settle, coordination facts
/// work, lane observations ask for attention, everything else is body text.
#[must_use]
pub fn fact_role(fact: &str) -> Role {
    match fact {
        "Seal"
        | "Supersede"
        | "GraphSeal"
        | "Integrate"
        | "AdmitEvidence"
        | "Resolve"
        | "Land"
        | "AdoptAnswer"
        | "BaseVerifyCompleted"
        | "BaseReverify"
        | "ProposeChange"
        | "StudyCompleted"
        | "ObserveMainline"
        | "ObserveMainlineDiverged"
        | "LandingRejected" => Role::Settled,
        _ if is_coordination_fact(fact) => Role::Working,
        _ if is_lane_fact(fact) => Role::Attention,
        _ => Role::Text,
    }
}

fn is_coordination_fact(fact: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "SharedRun",
        "Precheck",
        "Integration",
        "Candidate",
        "Compatibility",
        "Composition",
        "Splice",
        "StableHead",
        "ProofReused",
        "HoldShared",
        "PartialHead",
        "RequestConstruction",
        "ProposeSharedRun",
    ];
    let mut index = 0;
    while index < NEEDLES.len() {
        if contains(fact, NEEDLES[index]) {
            return true;
        }
        index += 1;
    }
    false
}

fn is_lane_fact(fact: &str) -> bool {
    matches!(
        fact,
        "AttemptCompleted"
            | "ConstructionCheckpointObserved"
            | "LaneWritesObserved"
            | "VerifyFailed"
            | "AggregateVerifyCompleted"
            | "AggregateReviewCompleted"
            | "MemberExecutorFault"
            | "FoldConflict"
            | "FoldRefused"
            | "ContainmentRefused"
            | "VerifyHostFault"
            | "ResumeHostFault"
            | "GrantAttempts"
            | "SurfaceRequested"
    )
}

fn contains(haystack: &str, needle: &str) -> bool {
    let hay = haystack.as_bytes();
    let pin = needle.as_bytes();
    if pin.len() > hay.len() {
        return false;
    }
    let mut start = 0;
    while start + pin.len() <= hay.len() {
        let mut matched = true;
        let mut offset = 0;
        while offset < pin.len() {
            if hay[start + offset] != pin[offset] {
                matched = false;
                break;
            }
            offset += 1;
        }
        if matched {
            return true;
        }
        start += 1;
    }
    false
}

/// Whether a refused or failed outcome paints in the warning colour.
#[must_use]
pub fn outcome_is_warning(outcome: &str) -> bool {
    ["Reject", "Refus", "Fail", "Wedge", "Fault"].iter().any(|needle| outcome.contains(needle))
}

/// Left-aligned fixed column: truncate with an ellipsis, else pad.
#[must_use]
pub fn format_cell(text: &str, width: usize) -> String {
    if text.chars().count() > width {
        truncate_ellipsis(text, width)
    } else {
        pad_end(text, width)
    }
}

/// Right-aligned fixed column for the sequence.
#[must_use]
pub fn format_cell_right(text: &str, width: usize) -> String {
    if text.chars().count() > width {
        truncate_ellipsis(text, width)
    } else {
        pad_start(text, width)
    }
}

/// Truncate to `width` characters with a trailing ellipsis, no padding.
#[must_use]
pub fn truncate_ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// The full fixed-column line search matches against, including the meaning.
#[must_use]
pub fn rendered_line(record: &JournalRecordView) -> String {
    let sequence = format_cell_right(&record.sequence.to_string(), SEQ_WIDTH);
    let time = format_cell(&recorded_time(record), TIME_WIDTH);
    let fact = format_cell(&fact_name(record), FACT_WIDTH);
    let bloom = format_cell(&bloom_prefix(record), BLOOM_WIDTH);
    let member = format_cell(&member_name(record), MEMBER_WIDTH);
    let outcome = format_cell(&outcome_name(record), OUTCOME_WIDTH);
    let mean = meaning(record);
    format!("{sequence} {time} {fact} {bloom} {member} {outcome} {mean}")
}

/// What this record means, in the reader's vocabulary.
#[must_use]
pub fn meaning(record: &JournalRecordView) -> String {
    let fact = fact_name(record);
    let outcome = outcome_name(record);
    match (fact.as_str(), outcome.as_str()) {
        ("Land", "Landed") => land_meaning(record),
        ("BaseVerifyCompleted", "BaseProven") => base_proven_meaning(record),
        ("BaseVerifyCompleted", "BaseRefused") => base_refused_meaning(record),
        ("BaseVerifyCompleted", _) => base_other_meaning(record, &outcome),
        ("SharedRunStarted", _) => shared_run_started_meaning(record, &outcome),
        ("SharedRunCompleted", _) => shared_run_completed_meaning(record, &outcome),
        ("AttemptCompleted", "AttemptAdvanced") => attempt_advanced_meaning(record),
        ("AttemptCompleted", "AttemptRetried") => attempt_retried_meaning(record),
        ("AttemptCompleted", "AttemptWedged") => attempt_wedged_meaning(record),
        ("AttemptCompleted", "AttemptParked") => attempt_parked_meaning(record),
        ("AttemptCompleted", "RefineReentered") => refine_reentered_meaning(record),
        ("AttemptCompleted", _) => attempt_other_meaning(record, &outcome),
        ("CompatibilityPreviewed", _) => compatibility_meaning(record),
        ("LaneWritesObserved", "LeasesObserved") => leases_meaning(record),
        ("LaneWritesObserved", _) => lane_other_meaning(record, &outcome),
        ("ConstructionCheckpointObserved", _) => checkpoint_meaning(record, &outcome),
        _ => outcome,
    }
}

fn land_meaning(record: &JournalRecordView) -> String {
    let outcome_body = outcome_body(record);
    let fact_body = fact_body(record);
    let previous = digest_field(outcome_body, "previous_base")
        .or_else(|| digest_field(outcome_body, "previous"))
        .unwrap_or_else(|| "????????".to_owned());
    let new = digest_field(outcome_body, "new_head")
        .or_else(|| digest_field(fact_body, "new_head"))
        .unwrap_or_else(|| "????????".to_owned());
    let members = count_field(outcome_body, &["resolution_claims", "members", "released", "lineage"])
        .or_else(|| count_field(fact_body, &["members", "resolution_claims"]));
    members.map_or_else(
        || format!("bloom landed: head {previous} → {new}"),
        |count| format!("bloom landed: head {previous} → {new}, {count} members"),
    )
}

fn base_proven_meaning(record: &JournalRecordView) -> String {
    let outcome_body = outcome_body(record);
    let fact_body = fact_body(record);
    let base = digest_field(outcome_body, "base")
        .or_else(|| digest_field(fact_body, "base"))
        .unwrap_or_else(|| "????????".to_owned());
    let tree = digest_field(outcome_body, "tree")
        .or_else(|| digest_field(fact_body, "tree"))
        .unwrap_or_else(|| "????????".to_owned());
    format!("base {base} proven green (tree {tree})")
}

fn base_refused_meaning(record: &JournalRecordView) -> String {
    let outcome_body = outcome_body(record);
    let fact_body = fact_body(record);
    let base = digest_field(outcome_body, "base")
        .or_else(|| digest_field(fact_body, "base"))
        .unwrap_or_else(|| "????????".to_owned());
    let tree = digest_field(outcome_body, "tree")
        .or_else(|| digest_field(fact_body, "tree"))
        .unwrap_or_else(|| "????????".to_owned());
    match joined_strings(outcome_body, "failed").or_else(|| joined_strings(fact_body, "failed")) {
        Some(failed) if !failed.is_empty() => format!("base {base} refused red (tree {tree}): {failed}"),
        _ => format!("base {base} refused red (tree {tree})"),
    }
}

fn base_other_meaning(record: &JournalRecordView, outcome: &str) -> String {
    let base = digest_field(outcome_body(record), "base")
        .or_else(|| digest_field(fact_body(record), "base"))
        .unwrap_or_else(|| "????????".to_owned());
    if outcome.contains("Queued") {
        return format!("base {base} verify queued");
    }
    outcome.to_owned()
}

fn shared_run_started_meaning(record: &JournalRecordView, outcome: &str) -> String {
    if outcome.contains("Reject") {
        let run = digest_field(fact_body(record), "run").unwrap_or_else(|| "????????".to_owned());
        return format!("run {run} refused: {outcome}");
    }
    let run = digest_field(fact_body(record), "run")
        .or_else(|| digest_field(outcome_body(record), "subject"))
        .unwrap_or_else(|| "????????".to_owned());
    let member = member_name(record);
    if member.is_empty() {
        let bloom = bloom_prefix(record);
        if bloom.is_empty() {
            format!("run {run} started")
        } else {
            format!("run {run} started for bloom {bloom}")
        }
    } else {
        format!("run {run} started for {member}")
    }
}

fn shared_run_completed_meaning(record: &JournalRecordView, outcome: &str) -> String {
    let body = fact_body(record);
    let run =
        digest_field(body, "run").or_else(|| completion_digest(body, "run")).unwrap_or_else(|| "????????".to_owned());
    let outcomes = array_len(body, "outcomes").or_else(|| completion_len(body, "outcomes")).unwrap_or(0);
    if outcomes > 0 {
        let unfinished = array_len(body, "unfinished").or_else(|| completion_len(body, "unfinished")).unwrap_or(0);
        if unfinished > 0 {
            return format!("run {run} completed: {outcomes} verdicts, {unfinished} unfinished");
        }
        return format!("run {run} completed: {outcomes} verdicts");
    }
    if outcome.contains("Reject") {
        let detail = rejection_detail(outcome);
        return format!("run {run} retired before a verdict: {detail}");
    }
    format!("run {run} retired before a verdict")
}

fn rejection_detail(outcome: &str) -> &str {
    if contains(outcome, "Mismatch") || contains(outcome, "Stale") || contains(outcome, "Expired") {
        "head moved"
    } else {
        "refused"
    }
}

fn attempt_advanced_meaning(record: &JournalRecordView) -> String {
    let outcome_body = outcome_body(record);
    let fact_body = fact_body(record);
    let member = str_field(outcome_body, "workpiece").or_else(|| str_field(fact_body, "workpiece")).unwrap_or("?");
    let from = str_field(outcome_body, "from").or_else(|| str_field(fact_body, "stage")).unwrap_or("?");
    let to = str_field(outcome_body, "to").unwrap_or("?");
    format!("{member} {from} passed → {to}")
}

fn attempt_retried_meaning(record: &JournalRecordView) -> String {
    let outcome_body = outcome_body(record);
    let fact_body = fact_body(record);
    let member = str_field(outcome_body, "workpiece").or_else(|| str_field(fact_body, "workpiece")).unwrap_or("?");
    let stage = str_field(outcome_body, "stage").or_else(|| str_field(fact_body, "stage")).unwrap_or("?");
    outcome_body.and_then(|body| body.get("attempt")).and_then(Value::as_u64).map_or_else(
        || format!("{member} {stage} failed, retrying"),
        |attempt| format!("{member} {stage} failed, retrying (attempt {attempt})"),
    )
}

fn attempt_wedged_meaning(record: &JournalRecordView) -> String {
    let member = str_field(outcome_body(record), "workpiece")
        .or_else(|| str_field(fact_body(record), "workpiece"))
        .unwrap_or("?");
    let stage =
        str_field(outcome_body(record), "stage").or_else(|| str_field(fact_body(record), "stage")).unwrap_or("?");
    format!("{member} {stage} wedged after retries")
}

fn attempt_parked_meaning(record: &JournalRecordView) -> String {
    let member = str_field(outcome_body(record), "workpiece")
        .or_else(|| str_field(fact_body(record), "workpiece"))
        .unwrap_or("?");
    format!("{member} parked for an owner decision")
}

fn refine_reentered_meaning(record: &JournalRecordView) -> String {
    let member = str_field(outcome_body(record), "workpiece")
        .or_else(|| str_field(fact_body(record), "workpiece"))
        .unwrap_or("?");
    format!("{member} back to Refine for repair")
}

fn attempt_other_meaning(record: &JournalRecordView, outcome: &str) -> String {
    let member = str_field(fact_body(record), "workpiece");
    member.map_or_else(|| outcome.to_owned(), |member| format!("{member} completion {outcome}"))
}

fn compatibility_meaning(record: &JournalRecordView) -> String {
    let body = fact_body(record);
    let result = body.and_then(|body| body.get("result"));
    let variant = result.and_then(Value::as_object).and_then(|map| map.keys().next().cloned()).unwrap_or_default();
    match variant.as_str() {
        "Refused" => "checkpoint does not merge onto the head yet (preview refused)".to_owned(),
        "Conflict" => "checkpoint conflicts with the head (preview conflict)".to_owned(),
        "Clean" => "checkpoint merges cleanly (preview clean)".to_owned(),
        _ => "checkpoint preview settled".to_owned(),
    }
}

fn leases_meaning(record: &JournalRecordView) -> String {
    let fact_body = fact_body(record);
    let outcome_body = outcome_body(record);
    let wrote = array_len(fact_body, "paths").unwrap_or(0);
    let paths = wrote == 1;
    let wrote_phrase = if paths {
        "lane wrote 1 path".to_owned()
    } else {
        format!("lane wrote {wrote} paths")
    };
    let acquired: Vec<String> = outcome_body
        .and_then(|body| body.get("acquired"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let acquired_phrase = match acquired.as_slice() {
        [] => "no new leases".to_owned(),
        [first] => format!("leases acquired on {first}"),
        [first, rest @ ..] => format!("leases acquired on {first} +{} more", rest.len()),
    };
    let evicted = outcome_body.and_then(|body| body.get("evicted")).and_then(Value::as_array).map_or(0, Vec::len);
    let evicted_phrase = if evicted == 0 {
        "none evicted".to_owned()
    } else {
        format!("{evicted} evicted")
    };
    format!("{wrote_phrase}; {acquired_phrase}, {evicted_phrase}")
}

fn lane_other_meaning(record: &JournalRecordView, outcome: &str) -> String {
    let member = member_name(record);
    if member.is_empty() {
        outcome.to_owned()
    } else {
        format!("{member} lane observation {outcome}")
    }
}

fn checkpoint_meaning(record: &JournalRecordView, outcome: &str) -> String {
    let member = checkpoint_workpiece(record).or_else(|| non_empty(member_name(record)));
    member.map_or_else(
        || format!("checkpoint published ({outcome})"),
        |member| format!("{member} checkpoint published ({outcome})"),
    )
}

fn checkpoint_workpiece(record: &JournalRecordView) -> Option<String> {
    fact_body(record)
        .and_then(|body| body.get("checkpoint"))
        .and_then(|checkpoint| checkpoint.get("workpiece"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

fn fact_body(record: &JournalRecordView) -> Option<&Value> {
    let fact = record.event.get("fact")?;
    let name = variant_name(&record.event, "fact");
    fact.get(&name).or(Some(fact))
}

fn outcome_body(record: &JournalRecordView) -> Option<&Value> {
    let name = variant_name(&record.outcome, "outcome");
    record.outcome.get(&name).or_else(|| record.outcome.get("outcome"))
}

fn str_field<'a>(body: Option<&'a Value>, key: &str) -> Option<&'a str> {
    body.and_then(|body| body.get(key)).and_then(Value::as_str)
}

fn digest_field(body: Option<&Value>, key: &str) -> Option<String> {
    digest_prefix(body?.get(key)?)
}

fn completion_digest(body: Option<&Value>, key: &str) -> Option<String> {
    digest_prefix(body?.get("completion")?.get(key)?)
}

fn array_len(body: Option<&Value>, key: &str) -> Option<usize> {
    body.and_then(|body| body.get(key)).and_then(Value::as_array).map(Vec::len)
}

fn completion_len(body: Option<&Value>, key: &str) -> Option<usize> {
    body.and_then(|body| body.get("completion"))
        .and_then(|completion| completion.get(key))
        .and_then(Value::as_array)
        .map(Vec::len)
}

fn count_field(body: Option<&Value>, keys: &[&str]) -> Option<usize> {
    let body = body?;
    for key in keys {
        if let Some(count) = body.get(*key).and_then(Value::as_array).map(Vec::len) {
            return Some(count);
        }
    }
    None
}

fn joined_strings(body: Option<&Value>, key: &str) -> Option<String> {
    let items = body?.get(key)?.as_array()?;
    let names: Vec<String> = items
        .iter()
        .map(|item| {
            item.as_str().map_or_else(
                || item.as_object().and_then(|map| map.keys().next().cloned()).unwrap_or_else(|| item.to_string()),
                str::to_owned,
            )
        })
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

fn digest_prefix(value: &Value) -> Option<String> {
    match value {
        Value::String(hex) if hex.len() >= 8 => Some(hex.chars().take(8).collect()),
        Value::Array(bytes) => {
            let raw: Option<Vec<u8>> = bytes.iter().map(|byte| u8::try_from(byte.as_u64()?).ok()).collect();
            let raw = raw?;
            (raw.len() == 32).then(|| hex_prefix(&raw))
        }
        Value::Object(map) if map.len() == 1 => digest_prefix(map.values().next()?),
        _ => None,
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(8);
    for byte in bytes.iter().take(4) {
        out.push(hex_nibble(byte >> 4));
        out.push(hex_nibble(byte & 0x0f));
    }
    out
}

const fn hex_nibble(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

fn variant_name(value: &Value, field: &str) -> String {
    let Some(obj) = value.as_object() else {
        return value.to_string();
    };
    if let Some(inner) = obj.get(field) {
        if let Some(name) = inner.as_object().and_then(|map| map.keys().next()) {
            return name.clone();
        }
        if let Some(name) = inner.as_str() {
            return name.to_owned();
        }
    }
    obj.keys().next().cloned().unwrap_or_else(|| value.to_string())
}

struct FactBits {
    bloom: Option<String>,
    member: Option<String>,
}

fn fact_bits(event: &Value) -> FactBits {
    let mut bits = FactBits { bloom: None, member: None };
    walk_fact(event, 0, &mut bits);
    bits
}

fn walk_fact(value: &Value, depth: usize, bits: &mut FactBits) {
    if depth > 6 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                walk_fact(item, depth.saturating_add(1), bits);
            }
        }
        Value::Object(obj) => {
            for (key, val) in obj {
                match key.as_str() {
                    "bloom" if bits.bloom.is_none() => bits.bloom = digest_prefix(val),
                    "workpiece" | "member" if bits.member.is_none() => {
                        if let Some(name) = val.as_str().filter(|name| !name.is_empty()) {
                            bits.member = Some(name.to_owned());
                        } else {
                            walk_fact(val, depth.saturating_add(1), bits);
                        }
                    }
                    _ => walk_fact(val, depth.saturating_add(1), bits),
                }
            }
        }
        _ => {}
    }
}

fn pad_end(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + width - count);
    out.push_str(text);
    for _ in count..width {
        out.push(' ');
    }
    out
}

fn pad_start(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + width - count);
    for _ in count..width {
        out.push(' ');
    }
    out.push_str(text);
    out
}
