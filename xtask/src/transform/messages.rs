//! The result record derived from an Anthropic Messages API wire-format
//! transcript — the richest envelope any arm produces, and the one two of them
//! share.
//!
//! Claude Code's `--output-format stream-json` and Grok Build's
//! `--output-format streaming-messages-json` are the same NDJSON dialect: a
//! stream of `assistant` message events and one terminal `result` event
//! carrying turns, duration, price, and token usage. So the derivation lives
//! beside neither arm and is read by both — a second copy would drift from the
//! ledger columns [`super::lane::record`] pins for the harnesses that report
//! less.

/// How much public assistant prose the result record retains. A Refine prompt
/// has a finite budget, and a transcript can carry tens of turns of narration;
/// the cap keeps the durable findings channel from becoming the transcript.
pub(super) const MAX_ASSISTANT_TEXT_BYTES: usize = 16 * 1024;

/// The Claude-harness tool a reviewer is told to file findings through. Its
/// `tool_use` payload is the structured findings channel; every other tool
/// stays out of the result record (#5118).
const REPORT_FINDINGS: &str = "ReportFindings";

/// The last `MAX_ASSISTANT_TEXT_BYTES` of `text`, snapped forward to a char
/// boundary. An over-budget transcript keeps the most recent prose — that is
/// where the critic's findings and verdict usually sit — rather than the
/// opening narration.
pub(super) fn bound_assistant_text(text: &str) -> String {
    const OMISSION: &str = "…\n";
    if text.len() <= MAX_ASSISTANT_TEXT_BYTES {
        return text.to_owned();
    }
    let budget = MAX_ASSISTANT_TEXT_BYTES.saturating_sub(OMISSION.len());
    let mut start = text.len().saturating_sub(budget);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{OMISSION}{}", &text[start..])
}

/// Pull public `type: "text"` blocks out of one assistant `message`. Tool-use
/// payloads, tool results, and non-public reasoning shapes are not text blocks
/// and do not survive here.
fn collect_public_text(message: &serde_json::Value, texts: &mut Vec<String>) {
    let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) else {
        return;
    };
    for block in blocks {
        if let Some(text) = public_text_block(block) {
            texts.push(text.to_owned());
        }
    }
}

fn public_text_block(block: &serde_json::Value) -> Option<&str> {
    if block.get("type").and_then(serde_json::Value::as_str) != Some("text") {
        return None;
    }
    block.get("text").and_then(serde_json::Value::as_str).filter(|text| !text.is_empty())
}

/// Collect `ReportFindings` `tool_use` payloads from one assistant `message`.
/// Matched against later `tool_result`s so a schema-rejected call cannot win.
fn collect_report_finding_uses(message: &serde_json::Value, pending: &mut Vec<(String, serde_json::Value)>) {
    let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
            continue;
        }
        if block.get("name").and_then(serde_json::Value::as_str) != Some(REPORT_FINDINGS) {
            continue;
        }
        let Some(id) = block.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(findings) = block.get("input").and_then(|input| input.get("findings")) else {
            continue;
        };
        if !findings.is_array() {
            continue;
        }
        pending.push((id.to_owned(), findings.clone()));
    }
}

/// Settle pending `ReportFindings` calls from one `user` event's `tool_result`s.
/// The last schema-accepted call wins; a validation error leaves `accepted` as
/// it was (#5118).
fn settle_report_findings(
    event: &serde_json::Value,
    pending: &mut Vec<(String, serde_json::Value)>,
    accepted: &mut Option<serde_json::Value>,
) {
    let Some(blocks) =
        event.get("message").and_then(|message| message.get("content")).and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(id) = block.get("tool_use_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(index) = pending.iter().rposition(|(pending_id, _)| pending_id == id) else {
            continue;
        };
        let (_, findings) = pending.remove(index);
        if !schema_rejected(block) {
            *accepted = Some(findings);
        }
    }
}

/// Whether this `tool_result` is a harness schema rejection. Those calls are
/// ignored so a malformed or over-budget payload cannot become the frozen
/// findings (#5118).
fn schema_rejected(block: &serde_json::Value) -> bool {
    if block.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
        return true;
    }
    block.get("content").and_then(serde_json::Value::as_str).is_some_and(|text| text.contains("InputValidationError"))
}

fn or_zero(value: &serde_json::Value, key: &str) -> serde_json::Value {
    value.get(key).cloned().unwrap_or_else(|| serde_json::json!(0))
}

fn or_null(value: &serde_json::Value, key: &str) -> serde_json::Value {
    value.get(key).cloned().unwrap_or(serde_json::Value::Null)
}

/// Fold the walked transcript pieces into the ledger-shaped result record.
fn assemble_result_record(
    result: Option<serde_json::Value>,
    first_main: Option<serde_json::Value>,
    calls: Vec<serde_json::Value>,
    texts: Vec<String>,
    report_findings: Option<serde_json::Value>,
) -> serde_json::Value {
    use serde_json::{Map, Value, json};

    let mut record = Map::new();
    record.insert("schema".to_owned(), json!(1));
    for field in ["task", "ref", "run_id", "conclusion", "model", "created_at", "pool"] {
        record.insert(field.to_owned(), Value::Null);
    }

    match &first_main {
        Some(first) => {
            let usage = first.get("usage").cloned().unwrap_or(Value::Null);
            record.insert("first_call_model".to_owned(), or_null(first, "model"));
            record.insert("first_call_cache_read".to_owned(), or_zero(&usage, "cache_read_input_tokens"));
            record.insert("first_call_cache_write".to_owned(), or_zero(&usage, "cache_creation_input_tokens"));
            record.insert("first_call_input".to_owned(), or_zero(&usage, "input_tokens"));
        }
        None => {
            for field in ["first_call_model", "first_call_cache_read", "first_call_cache_write", "first_call_input"] {
                record.insert(field.to_owned(), Value::Null);
            }
        }
    }

    record.insert(
        "calls".to_owned(),
        if calls.is_empty() {
            Value::Null
        } else {
            Value::Array(calls)
        },
    );
    record.insert(
        "assistant_text".to_owned(),
        if texts.is_empty() {
            Value::Null
        } else {
            json!(bound_assistant_text(&texts.join("\n\n")))
        },
    );
    record.insert(
        "report_findings".to_owned(),
        report_findings
            .filter(|findings| findings.as_array().is_some_and(|items| !items.is_empty()))
            .unwrap_or(Value::Null),
    );

    let Some(result) = result else {
        // A run that died before the terminal record — legible, cost unknown.
        record.insert("no_result".to_owned(), json!(true));
        return Value::Object(record);
    };
    let usage = result.get("usage").cloned().unwrap_or(Value::Null);
    let cache_creation = usage.get("cache_creation").cloned().unwrap_or(Value::Null);
    record.insert("num_turns".to_owned(), or_null(&result, "num_turns"));
    record.insert("cost_usd".to_owned(), or_null(&result, "total_cost_usd"));
    record.insert("duration_ms".to_owned(), or_null(&result, "duration_ms"));
    record.insert("is_error".to_owned(), or_null(&result, "is_error"));
    record.insert("input".to_owned(), or_zero(&usage, "input_tokens"));
    record.insert("cache_write".to_owned(), or_zero(&usage, "cache_creation_input_tokens"));
    record.insert("cache_write_1h".to_owned(), or_zero(&cache_creation, "ephemeral_1h_input_tokens"));
    record.insert("cache_write_5m".to_owned(), or_zero(&cache_creation, "ephemeral_5m_input_tokens"));
    record.insert("cache_read".to_owned(), or_zero(&usage, "cache_read_input_tokens"));
    record.insert("output".to_owned(), or_zero(&usage, "output_tokens"));
    record.insert("session_id".to_owned(), or_null(&result, "session_id"));
    // The terminal record whole — keeps every meter on disk for downstream study.
    record.insert("result".to_owned(), result);
    Value::Object(record)
}

/// Derive a model lane's result record from an Anthropic-Messages NDJSON
/// `transcript` — the in-repo, node-free replacement (#3572) for the retired
/// `scripts/agent-usage-record.mjs` shell-out (#3565 deletes that script). Pure,
/// and faithful to the ledger derivation: the terminal `result` event carries
/// cost / turns / duration / usage, and the first non-haiku assistant call's usage
/// is the warm-resume cache-hit signal. Public assistant text blocks from
/// non-side-model messages ride `assistant_text` so a review can retain findings
/// that never reached the terminal result (#5056). The last schema-accepted
/// `ReportFindings` `tool_use` rides `report_findings` so a review that filed
/// its findings through the harness tool still freezes them (#5118). A
/// transcript with no terminal `result` (a run that died early) yields a
/// `no_result` record rather than an error, so evidence is never dropped.
pub(super) fn derive_result_record(transcript: &str) -> serde_json::Value {
    use serde_json::{Value, json};

    let mut result: Option<Value> = None;
    let mut first_main: Option<Value> = None;
    let mut calls = Vec::new();
    let mut texts = Vec::new();
    let mut pending_report_findings = Vec::new();
    let mut report_findings = None;
    for line in transcript.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("result") => result = Some(event),
            Some("user") => settle_report_findings(&event, &mut pending_report_findings, &mut report_findings),
            Some("assistant") => {
                let message = event.get("message").cloned().unwrap_or(Value::Null);
                let model = message.get("model").and_then(Value::as_str).unwrap_or_default();
                if model.contains("haiku") {
                    continue;
                }
                collect_public_text(&message, &mut texts);
                collect_report_finding_uses(&message, &mut pending_report_findings);
                let Some(usage) = message.get("usage") else {
                    continue;
                };
                if first_main.is_none() {
                    first_main = Some(
                        json!({ "model": message.get("model").cloned().unwrap_or(Value::Null), "usage": usage.clone() }),
                    );
                }
                let cache_creation = usage.get("cache_creation").cloned().unwrap_or(Value::Null);
                calls.push(json!({
                    "input": or_zero(usage, "input_tokens"),
                    "cache_write": or_zero(usage, "cache_creation_input_tokens"),
                    "cache_write_1h": or_zero(&cache_creation, "ephemeral_1h_input_tokens"),
                    "cache_write_5m": or_zero(&cache_creation, "ephemeral_5m_input_tokens"),
                    "cache_read": or_zero(usage, "cache_read_input_tokens"),
                    "output": or_zero(usage, "output_tokens"),
                }));
            }
            _ => {}
        }
    }

    assemble_result_record(result, first_main, calls, texts, report_findings)
}

#[cfg(test)]
mod tests {
    use super::derive_result_record;

    // The result record is derived in-repo from the transcript — the node-free
    // replacement (#3572) for the `agent-usage-record.mjs` shell-out.
    #[test]
    fn result_record_derives_the_terminal_result_and_first_call_from_the_transcript() {
        let transcript = concat!(
            r#"{"type":"assistant","message":{"model":"claude-3-5-haiku","usage":{"input_tokens":9}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":7}}}"#,
            "\n",
            "\n",
            r#"{"type":"result","num_turns":3,"total_cost_usd":0.42,"is_error":false,"usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":50,"cache_creation":{"ephemeral_1h_input_tokens":11,"ephemeral_5m_input_tokens":22}}}"#,
        );
        let record = derive_result_record(transcript);
        assert_eq!(record["schema"], 1);
        assert_eq!(record["num_turns"], 3);
        assert_eq!(record["cost_usd"], 0.42);
        assert_eq!(record["is_error"], false);
        assert_eq!(record["output"], 200);
        assert_eq!(record["cache_write_5m"], 22, "the ephemeral-5m split is flattened out of cache_creation");
        // The first non-haiku assistant call is the warm-resume cache signal; the
        // haiku line before it is skipped.
        assert_eq!(record["first_call_model"], "claude-opus-4-8");
        assert_eq!(record["first_call_cache_read"], 40);
        assert_eq!(record["result"]["num_turns"], 3, "the terminal record is carried whole");
        // Haiku is a side model and is skipped; the opus call is the one the
        // ledger can band-bill. Flattening it into the aggregate alone would
        // leave the price table choosing one band for the whole dispatch.
        assert_eq!(record["calls"].as_array().map(Vec::len), Some(1));
        assert_eq!(record["calls"][0]["input"], 100);
        assert_eq!(record["calls"][0]["cache_read"], 40);
        assert!(record["assistant_text"].is_null(), "content-less assistant events contribute no public text");

        // A transcript with no terminal `result` is a legible `no_result` row,
        // never an error — evidence is never dropped.
        let died_early = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":5}}}"#;
        let partial = derive_result_record(died_early);
        assert_eq!(partial["no_result"], true);
        assert_eq!(partial["first_call_input"], 5);
        assert!(partial.get("cost_usd").is_none(), "a died-early row carries no cost columns");
        assert!(partial["assistant_text"].is_null());
    }

    fn transcript(events: &[serde_json::Value]) -> String {
        events.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
    }

    fn assistant(model: &str, content: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": {"model": model, "content": content, "usage": {"input_tokens": 1}},
        })
    }

    #[test]
    fn assistant_text_keeps_ordered_public_blocks_and_drops_everything_else() {
        // Tripwire (#5056): findings that never reach the terminal result still
        // have to be derivable, and a tool/reasoning payload that leaked here
        // would become repair instructions.
        let record = derive_result_record(&transcript(&[
            assistant("claude-3-5-haiku", &serde_json::json!([{"type": "text", "text": "SIDE_MODEL_TEXT"}])),
            assistant(
                "claude-opus-4-8",
                &serde_json::json!([
                    {"type": "thinking", "thinking": "HIDDEN_REASONING"},
                    {"type": "text", "text": "first finding: empty input panics."},
                    {"type": "tool_use", "name": "Read", "input": {"path": "TOOL_INPUT_SECRET"}},
                ]),
            ),
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "TOOL_RESULT_BODY"},
                ]},
            }),
            assistant(
                "claude-opus-4-8",
                &serde_json::json!([{"type": "text", "text": "second finding: the guard is untested."}]),
            ),
            serde_json::json!({"type": "system", "subtype": "init", "session_id": "s"}),
            serde_json::json!({"type": "result", "is_error": false, "result": "VERDICT: finding"}),
        ]));

        assert_eq!(
            record["assistant_text"],
            "first finding: empty input panics.\n\nsecond finding: the guard is untested.",
        );
        assert_eq!(record["result"]["result"], "VERDICT: finding", "metering still carries the terminal whole");
        let text = record["assistant_text"].as_str().expect("public text is retained");
        assert!(!text.contains("SIDE_MODEL_TEXT"), "haiku is a side model");
        assert!(!text.contains("HIDDEN_REASONING"), "thinking is not public text");
        assert!(!text.contains("TOOL_INPUT_SECRET"), "tool-use payloads stay out");
        assert!(!text.contains("TOOL_RESULT_BODY"), "tool results stay out");
        assert!(record["report_findings"].is_null(), "a Read tool is not ReportFindings");
    }

    #[test]
    fn assistant_text_is_bounded_to_the_most_recent_prose() {
        // Tripwire: an unbounded transcript would dump every turn of narration
        // into the result record and from there into Refine.
        let prefix = "a".repeat(super::MAX_ASSISTANT_TEXT_BYTES);
        let tail = "the last finding names the panic.";
        let record = derive_result_record(&transcript(&[assistant(
            "claude-opus-4-8",
            &serde_json::json!([{"type": "text", "text": format!("{prefix}{tail}")}]),
        )]));
        let text = record["assistant_text"].as_str().expect("over-budget prose is still retained");
        assert!(text.len() <= super::MAX_ASSISTANT_TEXT_BYTES, "the field itself is bounded: {}", text.len());
        assert!(text.ends_with(tail), "the most recent finding survives: {text}");
        assert!(text.starts_with('…'), "omission of the opening narration is marked");
        assert!(!text.contains(&prefix), "the opening pad does not survive whole");
    }

    #[test]
    fn report_findings_keeps_the_last_schema_accepted_call() {
        // Tripwire (#5118): a reviewer that follows the harness files findings
        // through ReportFindings, and a rejected call (malformed JSON, over-
        // budget short_summary) must not win over the later accepted one.
        let rejected = serde_json::json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result",
                "tool_use_id": "t-reject",
                "is_error": true,
                "content": "<tool_use_error>InputValidationError: short_summary too long</tool_use_error>",
            }]},
        });
        let accepted = serde_json::json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result",
                "tool_use_id": "t-accept",
                "content": "2 findings reported.",
            }]},
        });
        let record = derive_result_record(&transcript(&[
            assistant(
                "claude-opus-4-8",
                &serde_json::json!([{
                    "type": "tool_use",
                    "id": "t-reject",
                    "name": "ReportFindings",
                    "input": {"findings": [{"summary": "REJECTED — must not freeze"}]},
                }]),
            ),
            rejected,
            assistant(
                "claude-opus-4-8",
                &serde_json::json!([{
                    "type": "tool_use",
                    "id": "t-accept",
                    "name": "ReportFindings",
                    "input": {"findings": [
                        {"file": "src/lib.rs", "line": 10, "summary": "MECHANICAL — empty input panics"},
                        {"file": "src/names.rs", "line": 4, "summary": "JUDGMENT — the name is ugly"},
                    ]},
                }]),
            ),
            accepted,
            serde_json::json!({"type": "result", "is_error": false, "result": "VERDICT: finding"}),
        ]));

        let findings = record["report_findings"].as_array().expect("the last accepted call is retained");
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert_eq!(findings[0]["summary"], "MECHANICAL — empty input panics");
        assert_eq!(findings[1]["summary"], "JUDGMENT — the name is ugly");
        assert!(record["assistant_text"].is_null(), "a tool-only turn contributes no public text");

        let only_rejected = derive_result_record(&transcript(&[
            assistant(
                "claude-opus-4-8",
                &serde_json::json!([{
                    "type": "tool_use",
                    "id": "t-reject",
                    "name": "ReportFindings",
                    "input": {"findings": [{"summary": "REJECTED — must not freeze"}]},
                }]),
            ),
            serde_json::json!({
                "type": "user",
                "message": {"content": [{
                    "type": "tool_result",
                    "tool_use_id": "t-reject",
                    "is_error": true,
                    "content": "<tool_use_error>InputValidationError: JSON parse failed</tool_use_error>",
                }]},
            }),
        ]));
        assert!(
            only_rejected["report_findings"].is_null(),
            "a rejected call is not an accepted one: {}",
            only_rejected["report_findings"],
        );
    }
}
