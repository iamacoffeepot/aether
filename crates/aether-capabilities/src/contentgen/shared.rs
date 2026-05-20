//! Provider-agnostic helpers both content-gen backends share: the
//! configured `ureq` agent, the blocking request-run block, the
//! `status=<n>` error-string prefix parse, and the body-snippet trim.
//!
//! Extracted so the `aether.anthropic` and `aether.gemini` adapters
//! don't each carry a byte-identical copy of the HTTP plumbing (the
//! Qodana duplicate-code detector flags the parallel copies otherwise).
//! The error taxonomy stays per-provider — only the mechanical
//! string/HTTP scaffolding is shared.

use std::time::Duration;

use ureq::RequestExt;
use ureq::http::Request;

/// Build the shared `ureq` agent both backends use: HTTP error statuses
/// are surfaced as a normal response (`http_status_as_error(false)`) so
/// the caller maps the status onto the provider error taxonomy rather
/// than catching a `ureq::Error`.
#[must_use]
pub fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Run a built request through `agent` with a global timeout and return
/// `(status, retry_after_ms, body_text)`. The `retry-after` header (in
/// seconds) is converted to milliseconds when present. Errors are
/// free-form strings the caller maps onto its provider error taxonomy.
pub fn run_request(
    agent: &ureq::Agent,
    http_req: Request<Vec<u8>>,
    timeout: Duration,
) -> Result<(u16, Option<u32>, String), String> {
    let mut response = http_req
        .with_agent(agent)
        .configure()
        .timeout_global(Some(timeout))
        .build()
        .run()
        .map_err(|e| format!("request: {e}"))?;
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
    Ok((status, retry_after_ms, text))
}

/// Parse the `<status> retry_after_ms=<Debug-of-Option<u32>>` prefix a
/// backend prepends to a non-2xx error string (after the caller strips
/// the leading `status=`). Returns `(status, retry_after_ms)` on a
/// clean parse. Both providers format the prefix identically.
#[must_use]
pub fn parse_status_prefix(rest: &str) -> Option<(u16, Option<u32>)> {
    let mut parts = rest.split_whitespace();
    let status = parts.next()?.parse::<u16>().ok()?;
    let retry_after_ms = parts.next().and_then(|tok| {
        tok.strip_prefix("retry_after_ms=").and_then(|v| {
            // The backend formats `Option<u32>` via Debug — `Some(1500)`
            // or `None`. Extract the inner integer when present.
            v.strip_prefix("Some(")
                .and_then(|s| s.strip_suffix(')'))
                .and_then(|n| n.parse::<u32>().ok())
        })
    });
    Some((status, retry_after_ms))
}

/// Trim a response body to a short diagnostic snippet so an adapter
/// error message stays log-sized even when the provider returns a
/// multi-kilobyte error page. Truncates on a char boundary.
#[must_use]
pub fn snippet(body: &str) -> String {
    const MAX: usize = 256;
    if body.len() <= MAX {
        body.to_string()
    } else {
        let mut end = MAX;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &body[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_status_prefix, snippet};

    #[test]
    fn parse_status_prefix_extracts_status_and_retry() {
        assert_eq!(
            parse_status_prefix("429 retry_after_ms=Some(1500) body=x"),
            Some((429, Some(1500)))
        );
        assert_eq!(
            parse_status_prefix("500 retry_after_ms=None body=oops"),
            Some((500, None))
        );
        assert_eq!(parse_status_prefix("not-a-status"), None);
    }

    #[test]
    fn snippet_truncates_on_char_boundary() {
        let long = "x".repeat(1000);
        let s = snippet(&long);
        assert!(s.len() <= 260);
        assert!(s.ends_with('…'));
        assert_eq!(snippet("short"), "short");
    }
}
