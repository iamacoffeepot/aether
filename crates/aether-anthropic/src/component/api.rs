//! Messages-API request-body building + success-response parsing for the guest
//! `aether.anthropic` component (ADR-0159 §2).
//!
//! Ported byte-for-byte from the native `api.rs`: the same
//! `POST https://api.anthropic.com/v1/messages` body shape and the same
//! success-response parse, lifted off the `ureq` transport so they run
//! guest-side over the `FetchResult` the `aether.http` cap replies. Response
//! parsing stays factored so a fixture-replay test locks the vendor wire shape
//! without a network round-trip (ADR-0050 §4).

use serde_json::{Value, json};

use aether_kinds::Usage;

/// Default `max_tokens` when the request omits it. The Messages API requires
/// `max_tokens`, so the component supplies a conservative default rather than
/// rejecting the request — identical to the native backend.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Build the JSON request body for a Messages completion. Ported from the
/// native `build_request_body`; takes the flattened prompt plus the completion
/// knobs directly rather than through the retired transport's request struct.
pub fn build_request_body(
    model: &str,
    prompt: &str,
    system: Option<&str>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": [{ "role": "user", "content": prompt }],
    });
    if let Some(system) = system {
        body["system"] = json!(system);
    }
    if let Some(temperature) = temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

/// A parsed Messages-API success response. `wall_clock_millis` is not measured
/// guest-side — the guest holds no clock across the async edge round-trip — so
/// [`Usage::wall_clock_millis`] rides `0`; the token counts still carry.
pub struct ParsedResponse {
    pub text: String,
    pub model_used: String,
    pub usage: Usage,
}

/// Parse a Messages-API success response. Ported from the native
/// `parse_messages_response`: concatenates the `text` fields of every
/// `text`-typed content block, reads the served model (falling back to the
/// requested `fallback_model`) and the token usage.
pub fn parse_messages_response(json: &str, fallback_model: &str) -> Result<ParsedResponse, String> {
    let parsed: Value = serde_json::from_str(json).map_err(|e| format!("parse response: {e}"))?;

    let text = parsed
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .ok_or_else(|| "response missing content array".to_string())?;

    let model_used = parsed.get("model").and_then(Value::as_str).unwrap_or(fallback_model).to_string();

    let usage = parsed.get("usage");
    let input_tokens = usage.and_then(|u| u.get("input_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let output_tokens = usage.and_then(|u| u.get("output_tokens")).and_then(Value::as_u64).unwrap_or(0);

    Ok(ParsedResponse {
        text,
        model_used,
        usage: Usage {
            input_tokens: clamp_u32(input_tokens),
            output_tokens: clamp_u32(output_tokens),
            wall_clock_millis: 0,
            cost_micros: None,
        },
    })
}

fn clamp_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{build_request_body, parse_messages_response};

    /// Fixture-replay: a captured Messages-API success response. Locks the
    /// shape `parse_messages_response` reads (ADR-0050 §4) so a vendor wire
    /// drift is caught here, not at runtime. Shares the native backend's
    /// fixture file — the two parsers read the identical wire shape.
    const FIXTURE: &str = include_str!("../fixtures/messages_response.json");

    #[test]
    fn parses_fixture_response() {
        let resp =
            parse_messages_response(FIXTURE, "fallback-model").expect("fixture is a valid Messages-API response");
        assert_eq!(resp.text, "Hello! How can I help you today?");
        assert_eq!(resp.model_used, "claude-opus-4-7");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 9);
    }

    #[test]
    fn concatenates_multiple_text_blocks() {
        let json = r#"{
            "content": [
                {"type": "text", "text": "part one "},
                {"type": "tool_use", "id": "x"},
                {"type": "text", "text": "part two"}
            ],
            "model": "m",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }"#;
        let resp = parse_messages_response(json, "m").expect("multi-block response parses");
        assert_eq!(resp.text, "part one part two");
    }

    #[test]
    fn missing_content_array_errors() {
        let json = r#"{"model": "m", "usage": {}}"#;
        assert!(parse_messages_response(json, "m").is_err());
    }

    #[test]
    fn missing_model_falls_back_to_requested() {
        let json = r#"{"content": [{"type": "text", "text": "x"}]}"#;
        let resp = parse_messages_response(json, "requested-model").expect("response without a model field parses");
        assert_eq!(resp.model_used, "requested-model");
    }

    #[test]
    fn request_body_carries_model_and_default_max_tokens() {
        let body = build_request_body("claude-test", "hi", Some("be terse"), None, Some(0.5));
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
    }
}
