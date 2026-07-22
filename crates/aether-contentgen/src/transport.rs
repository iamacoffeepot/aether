//! Provider-agnostic `ureq` plumbing both content-gen backends share: the
//! configured `ureq` agent and the blocking request-run block.
//!
//! Extracted so the `aether.anthropic` and `aether.gemini` adapters
//! don't each carry a byte-identical copy of the HTTP plumbing (the
//! duplicate-code check flags the parallel copies otherwise).
//! The error taxonomy stays per-provider — only the mechanical
//! string/HTTP scaffolding is shared.
//!
//! The I/O-free `status=<n>` prefix parse and body-snippet trim moved to the
//! always-on [`strparse`](crate::strparse) module (ADR-0159 §2) so a guest
//! provider component reuses them without the `ureq` runtime; they are
//! re-exported here so existing `transport::{parse_status_prefix, snippet}`
//! callers stay unchanged.

use std::time::Duration;

use ureq::RequestExt;
use ureq::http::Request;

pub use crate::strparse::{parse_status_prefix, snippet};

/// Build the shared `ureq` agent both backends use: HTTP error statuses
/// are surfaced as a normal response (`http_status_as_error(false)`) so
/// the caller maps the status onto the provider error taxonomy rather
/// than catching a `ureq::Error`.
#[must_use]
pub fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder().http_status_as_error(false).build();
    ureq::Agent::new_with_config(config)
}

/// Run a built request through `agent` with a global timeout and return
/// `(status, retry_after_millis, body_text)`. The `retry-after` header (in
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
    let retry_after_millis = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|secs| secs.saturating_mul(1000));
    let text = response.body_mut().read_to_string().map_err(|e| format!("read body: {e}"))?;
    Ok((status, retry_after_millis, text))
}
