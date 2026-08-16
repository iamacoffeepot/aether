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

/// Derive a model lane's result record from an Anthropic-Messages NDJSON
/// `transcript` — the in-repo, node-free replacement (#3572) for the retired
/// `scripts/agent-usage-record.mjs` shell-out (#3565 deletes that script). Pure,
/// and faithful to the ledger derivation: the terminal `result` event carries
/// cost / turns / duration / usage, and the first non-haiku assistant call's usage
/// is the warm-resume cache-hit signal. Public assistant text blocks from
/// non-side-model messages ride `assistant_text` so a review can retain findings
/// that never reached the terminal result (#5056). A transcript with no terminal
/// `result` (a run that died early) yields a `no_result` record rather than an
/// error, so evidence is never dropped.
pub(super) fn derive_result_record(transcript: &str) -> serde_json::Value {
    use serde_json::{Map, Value, json};

    let or_zero = |value: &Value, key: &str| value.get(key).cloned().unwrap_or_else(|| json!(0));
    let or_null = |value: &Value, key: &str| value.get(key).cloned().unwrap_or(Value::Null);

    let mut result: Option<Value> = None;
    let mut first_main: Option<Value> = None;
    let mut calls = Vec::new();
    let mut texts = Vec::new();
    for line in transcript.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("result") => result = Some(event),
            Some("assistant") => {
                let message = event.get("message").cloned().unwrap_or(Value::Null);
                let model = message.get("model").and_then(Value::as_str).unwrap_or_default();
                if model.contains("haiku") {
                    continue;
                }
                collect_public_text(&message, &mut texts);
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
}
