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

/// Derive a model lane's result record from an Anthropic-Messages NDJSON
/// `transcript` — the in-repo, node-free replacement (#3572) for the retired
/// `scripts/agent-usage-record.mjs` shell-out (#3565 deletes that script). Pure,
/// and faithful to the ledger derivation: the terminal `result` event carries
/// cost / turns / duration / usage, and the first non-haiku assistant call's usage
/// is the warm-resume cache-hit signal. A transcript with no terminal `result` (a
/// run that died early) yields a `no_result` record rather than an error, so
/// evidence is never dropped.
pub(super) fn derive_result_record(transcript: &str) -> serde_json::Value {
    use serde_json::{Map, Value, json};

    let or_zero = |value: &Value, key: &str| value.get(key).cloned().unwrap_or_else(|| json!(0));
    let or_null = |value: &Value, key: &str| value.get(key).cloned().unwrap_or(Value::Null);

    let mut result: Option<Value> = None;
    let mut first_main: Option<Value> = None;
    let mut calls = Vec::new();
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

        // A transcript with no terminal `result` is a legible `no_result` row,
        // never an error — evidence is never dropped.
        let died_early = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":5}}}"#;
        let partial = derive_result_record(died_early);
        assert_eq!(partial["no_result"], true);
        assert_eq!(partial["first_call_input"], 5);
        assert!(partial.get("cost_usd").is_none(), "a died-early row carries no cost columns");
    }
}
