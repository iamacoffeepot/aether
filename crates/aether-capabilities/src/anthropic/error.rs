//! HTTP-status / refusal → [`AnthropicError`] mapping for the
//! `aether.anthropic` cap (ADR-0050 §1).
//!
//! The Messages-API backend (`api.rs`) calls [`status_to_error`] when
//! the provider returns a non-2xx status; the CLI backend maps its own
//! failure modes inline. Keeping the status table here keeps the
//! provider-compat translation in one place per ADR-0050 §4.

use crate::anthropic::kinds::AnthropicError;

use crate::anthropic::cli::{CLI_NOT_FOUND, TIMEOUT_SENTINEL};
use crate::shared::contentgen::shared::{parse_status_prefix, snippet};

/// Sentinel an adapter returns to mean "no API key" so the cap maps it
/// onto [`AnthropicError::Unauthorized`] without the adapter depending
/// on the kind enum. The `DisabledAnthropicAdapter` returns this for
/// Messages requests.
pub const UNAUTHORIZED_SENTINEL: &str = "unauthorized";

/// Convert a free-form adapter error string into the typed
/// [`AnthropicError`]. Recognises the structured sentinels the
/// backends emit (the CLI's `CLI_NOT_FOUND`, the disabled adapter's
/// `UNAUTHORIZED_SENTINEL`, and the `status=<n>` prefix the Messages
/// backend uses) and falls back to `AdapterError` for everything else.
#[must_use]
pub fn adapter_error_to_typed(raw: &str) -> AnthropicError {
    if raw == CLI_NOT_FOUND {
        return AnthropicError::CliNotFound;
    }
    if raw == UNAUTHORIZED_SENTINEL {
        return AnthropicError::Unauthorized;
    }
    if let Some(rest) = raw.strip_prefix(TIMEOUT_SENTINEL) {
        // `timeout=<elapsed_millis>` — parse the trailing integer the way
        // the `status=` prefix is parsed. A malformed tail falls back to
        // 0 rather than dropping the timeout classification.
        let elapsed_millis = rest.trim().parse::<u32>().unwrap_or(0);
        return AnthropicError::Timeout { elapsed_millis };
    }
    if let Some(rest) = raw.strip_prefix("status=")
        && let Some((status, retry_after_millis)) = parse_status_prefix(rest)
    {
        return status_to_error(status, retry_after_millis, rest);
    }
    AnthropicError::AdapterError(snippet(raw))
}

/// Map an HTTP status code from the Messages API onto an
/// [`AnthropicError`]. `body` is the response text, threaded through so
/// `AdapterError` can preserve provider diagnostics for the status
/// codes that don't have a typed variant.
///
/// - `401` / `403` → `Unauthorized` (bad / missing key)
/// - `429` → `RateLimited` (the `retry_after_millis` is parsed from the
///   `retry-after` header by the caller and threaded in)
/// - `529` → `Overloaded` (Anthropic's "service overloaded")
/// - everything else non-2xx → `AdapterError` carrying the status +
///   body snippet
#[must_use]
pub fn status_to_error(status: u16, retry_after_millis: Option<u32>, body: &str) -> AnthropicError {
    match status {
        401 | 403 => AnthropicError::Unauthorized,
        429 => AnthropicError::RateLimited { retry_after_millis },
        529 => AnthropicError::Overloaded,
        other => AnthropicError::AdapterError(format!("http {other}: {}", snippet(body))),
    }
}

#[cfg(test)]
mod tests {
    use super::{adapter_error_to_typed, status_to_error};
    use crate::anthropic::cli::TIMEOUT_SENTINEL;
    use crate::anthropic::kinds::AnthropicError;

    #[test]
    fn timeout_sentinel_maps_to_timeout() {
        let raw = format!("{TIMEOUT_SENTINEL}1500");
        assert_eq!(
            adapter_error_to_typed(&raw),
            AnthropicError::Timeout {
                elapsed_millis: 1500
            }
        );
    }

    #[test]
    fn malformed_timeout_sentinel_still_classifies_as_timeout() {
        let raw = format!("{TIMEOUT_SENTINEL}garbage");
        assert_eq!(
            adapter_error_to_typed(&raw),
            AnthropicError::Timeout { elapsed_millis: 0 }
        );
    }

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
                retry_after_millis: Some(1500)
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
}
