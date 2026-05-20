//! HTTP-status / refusal → [`AnthropicError`] mapping for the
//! `aether.anthropic` cap (ADR-0050 §1).
//!
//! The Messages-API backend (`api.rs`) calls [`status_to_error`] when
//! the provider returns a non-2xx status; the CLI backend maps its own
//! failure modes inline. Keeping the status table here keeps the
//! provider-compat translation in one place per ADR-0050 §4.

use aether_kinds::AnthropicError;

use crate::anthropic::cli::CLI_NOT_FOUND;

/// Sentinel an adapter returns to mean "no API key" so the cap maps it
/// onto [`AnthropicError::Unauthorized`] without the adapter depending
/// on the kind enum. The `DisabledAnthropicAdapter` returns this for
/// Messages requests.
pub const UNAUTHORIZED_SENTINEL: &str = "unauthorized";

/// Convert a free-form adapter error string into the typed
/// [`AnthropicError`]. Recognises the structured sentinels the
/// backends emit (the CLI's [`CLI_NOT_FOUND`], the disabled adapter's
/// [`UNAUTHORIZED_SENTINEL`], and the `status=<n>` prefix the Messages
/// backend uses) and falls back to `AdapterError` for everything else.
#[must_use]
pub fn adapter_error_to_typed(raw: &str) -> AnthropicError {
    if raw == CLI_NOT_FOUND {
        return AnthropicError::CliNotFound;
    }
    if raw == UNAUTHORIZED_SENTINEL {
        return AnthropicError::Unauthorized;
    }
    if let Some(rest) = raw.strip_prefix("status=")
        && let Some((status, retry_after_ms)) = parse_status_prefix(rest)
    {
        return status_to_error(status, retry_after_ms, rest);
    }
    AnthropicError::AdapterError(snippet(raw))
}

/// Parse the `status=<n> retry_after_ms=<...>` prefix the Messages
/// backend prepends to a non-2xx error string. Returns
/// `(status, retry_after_ms)` on a clean parse.
fn parse_status_prefix(rest: &str) -> Option<(u16, Option<u32>)> {
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

/// Map an HTTP status code from the Messages API onto an
/// [`AnthropicError`]. `body` is the response text, threaded through so
/// `AdapterError` can preserve provider diagnostics for the status
/// codes that don't have a typed variant.
///
/// - `401` / `403` → `Unauthorized` (bad / missing key)
/// - `429` → `RateLimited` (the `retry_after_ms` is parsed from the
///   `retry-after` header by the caller and threaded in)
/// - `529` → `Overloaded` (Anthropic's "service overloaded")
/// - everything else non-2xx → `AdapterError` carrying the status +
///   body snippet
#[must_use]
pub fn status_to_error(status: u16, retry_after_ms: Option<u32>, body: &str) -> AnthropicError {
    match status {
        401 | 403 => AnthropicError::Unauthorized,
        429 => AnthropicError::RateLimited { retry_after_ms },
        529 => AnthropicError::Overloaded,
        other => AnthropicError::AdapterError(format!("http {other}: {}", snippet(body))),
    }
}

/// Trim a response body to a short diagnostic snippet so an
/// `AdapterError` message stays log-sized even when the provider
/// returns a multi-kilobyte error page.
fn snippet(body: &str) -> String {
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
    use super::{snippet, status_to_error};
    use aether_kinds::AnthropicError;

    #[test]
    fn unauthorized_statuses_map_to_unauthorized() {
        assert_eq!(status_to_error(401, None, ""), AnthropicError::Unauthorized);
        assert_eq!(status_to_error(403, None, ""), AnthropicError::Unauthorized);
    }

    #[test]
    fn rate_limit_threads_retry_after() {
        assert_eq!(
            status_to_error(429, Some(1500), "slow down"),
            AnthropicError::RateLimited {
                retry_after_ms: Some(1500)
            }
        );
    }

    #[test]
    fn overloaded_status_maps_to_overloaded() {
        assert_eq!(status_to_error(529, None, ""), AnthropicError::Overloaded);
    }

    #[test]
    fn other_status_carries_adapter_error() {
        let err = status_to_error(500, None, "internal error");
        let AnthropicError::AdapterError(msg) = err else {
            panic!("expected AdapterError, got {err:?}");
        };
        assert!(msg.contains("500"));
        assert!(msg.contains("internal error"));
    }

    #[test]
    fn snippet_truncates_long_bodies_on_char_boundary() {
        let long = "x".repeat(1000);
        let s = snippet(&long);
        assert!(s.len() <= 260);
        assert!(s.ends_with('…'));
    }
}
