//! `ureq`-backed Anthropic Messages-API backend for the
//! `aether.anthropic` cap (ADR-0050). `POST
//! https://api.anthropic.com/v1/messages` — the Messages API, not the
//! deprecated `/v1/complete`, so the reply field is `text`, never
//! `completion`.
//!
//! The blocking `ureq` call runs on issue 1013's spawn-and-die
//! ephemeral thread, never on the dispatcher. Response parsing is
//! factored into [`parse_messages_response`] so a fixture-replay test
//! can lock the API response shape without a network round-trip
//! (ADR-0050 §4).

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ureq::http::Request;

use crate::contentgen::adapter::{AdapterUsage, AnthropicRequest, AnthropicResponse};

/// Official Messages API endpoint.
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// Anthropic API version header value. Pinned per the public Messages
/// API contract; bump when the cap is verified against a newer version.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default `max_tokens` when the request omits it. The Messages API
/// requires `max_tokens`, so the cap supplies a conservative default
/// rather than rejecting the request.
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// `ureq`-backed Messages-API adapter. Holds the shared agent, the API
/// key, and the per-request timeout. Thread-safe: `ureq::Agent` is
/// cheaply cloneable and internally synchronised, so each ephemeral
/// dispatch thread clones the agent off the cap's stored adapter.
pub struct UreqAnthropicAdapter {
    agent: ureq::Agent,
    api_key: String,
    timeout: Duration,
}

impl UreqAnthropicAdapter {
    /// Build an adapter with an explicit key + timeout. Chassis code
    /// resolves the key from `ANTHROPIC_API_KEY`; tests build directly.
    #[must_use]
    pub fn new(api_key: String, timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            api_key,
            timeout,
        }
    }

    /// Run a Messages completion. Returns the parsed response or a
    /// free-form error string the cap maps onto `AnthropicError`.
    pub fn messages_send(&self, req: &AnthropicRequest) -> Result<AnthropicResponse, String> {
        use ureq::RequestExt;

        let started = Instant::now();
        let body = build_request_body(req);
        let body_bytes = serde_json::to_vec(&body).map_err(|e| format!("encode request: {e}"))?;

        let http_req = Request::builder()
            .method("POST")
            .uri(MESSAGES_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header(
                "user-agent",
                concat!("aether/", env!("CARGO_PKG_VERSION")),
            )
            .body(body_bytes)
            .map_err(|e| format!("build request: {e}"))?;

        let mut response = http_req
            .with_agent(&self.agent)
            .configure()
            .timeout_global(Some(self.timeout))
            .build()
            .run()
            .map_err(|e| format!("messages request: {e}"))?;

        let status = response.status().as_u16();
        let retry_after_ms = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|secs| secs.saturating_mul(1000));
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("read body: {e}"))?;

        if !(200..300).contains(&status) {
            // Encode the status + retry-after into the error string so
            // the cap's `error::status_to_error` mapping reconstructs
            // the typed variant. The cap parses the leading status.
            return Err(format!("status={status} retry_after_ms={retry_after_ms:?} body={text}"));
        }

        let elapsed_ms = elapsed_ms(started);
        parse_messages_response(&text, &req.model, elapsed_ms)
    }
}

/// Build the JSON request body for a Messages completion.
fn build_request_body(req: &AnthropicRequest) -> Value {
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": [{ "role": "user", "content": req.prompt }],
    });
    if let Some(system) = &req.system {
        body["system"] = json!(system);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

/// Parse a Messages-API success response into an [`AnthropicResponse`].
/// Concatenates the `text` fields of every `text`-typed content block;
/// reads the model the provider served (falling back to the requested
/// `fallback_model`) and the token usage. Factored out so a
/// fixture-replay test locks the response shape (ADR-0050 §4).
pub fn parse_messages_response(
    json: &str,
    fallback_model: &str,
    wall_clock_ms: u32,
) -> Result<AnthropicResponse, String> {
    let parsed: Value = serde_json::from_str(json).map_err(|e| format!("parse response: {e}"))?;

    let text = parsed
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .ok_or_else(|| "response missing content array".to_string())?;

    let model_used = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(fallback_model)
        .to_string();

    let usage_obj = parsed.get("usage");
    let input_tokens = usage_obj
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage_obj
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    Ok(AnthropicResponse {
        text,
        model_used,
        usage: AdapterUsage {
            input_tokens: clamp_u32(input_tokens),
            output_tokens: clamp_u32(output_tokens),
            wall_clock_ms,
            cost_micros: None,
        },
    })
}

fn clamp_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn elapsed_ms(started: Instant) -> u32 {
    clamp_u32(u64::try_from(started.elapsed().as_millis()).unwrap_or_else(|_| u64::from(u32::MAX)))
}

#[cfg(test)]
mod tests {
    use super::{build_request_body, parse_messages_response};
    use crate::contentgen::adapter::AnthropicRequest;

    /// Fixture-replay: a captured Messages-API success response. Locks
    /// the shape `parse_messages_response` reads (ADR-0050 §4) so a
    /// vendor wire-format drift is caught here, not at runtime.
    const FIXTURE: &str = include_str!("fixtures/messages_response.json");

    #[test]
    fn parses_fixture_response() {
        let resp = parse_messages_response(FIXTURE, "fallback-model", 42)
            .expect("fixture is a valid Messages-API response");
        assert_eq!(resp.text, "Hello! How can I help you today?");
        assert_eq!(resp.model_used, "claude-opus-4-20250514");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 9);
        assert_eq!(resp.usage.wall_clock_ms, 42);
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
        let resp =
            parse_messages_response(json, "m", 0).expect("multi-block response parses");
        assert_eq!(resp.text, "part one part two");
    }

    #[test]
    fn missing_content_array_errors() {
        let json = r#"{"model": "m", "usage": {}}"#;
        assert!(parse_messages_response(json, "m", 0).is_err());
    }

    #[test]
    fn missing_model_falls_back_to_requested() {
        let json = r#"{"content": [{"type": "text", "text": "x"}]}"#;
        let resp = parse_messages_response(json, "requested-model", 0)
            .expect("response without a model field parses");
        assert_eq!(resp.model_used, "requested-model");
    }

    #[test]
    fn request_body_carries_model_and_default_max_tokens() {
        let req = AnthropicRequest {
            model: "claude-test".to_string(),
            prompt: "hi".to_string(),
            system: Some("be terse".to_string()),
            max_tokens: None,
            temperature: Some(0.5),
        };
        let body = build_request_body(&req);
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
    }
}
