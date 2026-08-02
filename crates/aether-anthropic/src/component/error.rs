//! Edge-outcome → [`AnthropicError`] mapping for the guest component
//! (ADR-0159 / ADR-0157 / ADR-0158).
//!
//! The status → taxonomy table ports byte-for-byte from the native `error.rs`
//! ([`status_to_error`]). What changes is the boundary: the native adapter
//! surfaced failures as a free-form `Result<_, String>` that `error.rs` then
//! re-parsed through `status=` / `timeout=` / `cli-not-found` sentinels. The
//! guest reads the *typed* [`aether_http::FetchResult`] /
//! [`aether_process::RunResult`] edge replies, so the
//! sentinel round-trip is gone — each typed arm maps straight into the
//! taxonomy.
//!
//! The CLI re-fold is the one semantic subtlety ADR-0157 records: a non-zero
//! `claude` exit arrives as [`aether_process::RunResult::Ok`] (a completed run the caller
//! judges), so the guest re-folds a non-success exit into a provider
//! [`AnthropicError::AdapterError`] exactly as the native `cli.rs` did.

use aether_http::{HttpError, HttpHeader};
use aether_process::ProcessError;

use crate::kinds::AnthropicError;

/// Truncate a body snippet for a diagnostic error message. Ported from
/// `transport::snippet` — 256 chars, char-boundary safe, ellipsis on overflow.
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

/// Map a non-2xx Messages-API status onto an [`AnthropicError`]. Ported table:
///
/// - `401` / `403` → `Unauthorized` (bad / missing key)
/// - `429` → `RateLimited` (the `retry_after_millis` parsed from the
///   `retry-after` response header by [`retry_after_millis`])
/// - `529` → `Overloaded` (Anthropic's "service overloaded")
/// - everything else non-2xx → `AdapterError` carrying the status + body
///   snippet
pub fn status_to_error(status: u16, retry_after_millis: Option<u32>, body: &str) -> AnthropicError {
    match status {
        401 | 403 => AnthropicError::Unauthorized,
        429 => AnthropicError::RateLimited { retry_after_millis },
        529 => AnthropicError::Overloaded,
        other => AnthropicError::AdapterError(format!("http {other}: {}", snippet(body))),
    }
}

/// Parse the `retry-after` response header (seconds) into milliseconds for a
/// `429`. The native path parsed the same header inside the transport and
/// threaded the millis through; the guest reads it off the `FetchResult`
/// headers directly.
pub fn retry_after_millis(headers: &[HttpHeader]) -> Option<u32> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("retry-after"))
        .and_then(|h| h.value.trim().parse::<u64>().ok())
        .and_then(|secs| u32::try_from(secs.saturating_mul(1000)).ok())
}

/// Map an `aether.http` transport failure onto the taxonomy. A DNS / TLS /
/// connection failure has no typed anthropic variant, so it preserves the
/// backend detail as `AdapterError` — the native transport's failure strings
/// landed there too.
pub fn http_error_to_typed(error: HttpError) -> AnthropicError {
    match error {
        HttpError::Timeout => AnthropicError::Timeout { elapsed_millis: 0 },
        HttpError::InvalidUrl(url) => AnthropicError::AdapterError(format!("invalid url: {url}")),
        HttpError::BodyTooLarge => AnthropicError::AdapterError("response body exceeded the egress cap".to_string()),
        HttpError::AllowlistDenied => {
            AnthropicError::AdapterError("http egress not permitted for the Messages API host".to_string())
        }
        HttpError::Disabled => AnthropicError::AdapterError("http egress is disabled".to_string()),
        HttpError::AdapterError(detail) => AnthropicError::AdapterError(snippet(&detail)),
    }
}

/// Map an `aether.process` refusal onto the taxonomy (ADR-0157 §Mail surface).
/// The allowlist refusal and the missing-binary path both fold into
/// `CliNotFound` — the graceful "the `claude` backend isn't available" skip the
/// kind already models — while an exec / wait failure preserves its OS detail
/// as `AdapterError`, exactly as the native `cli.rs` spawn/wait errors did.
pub fn process_error_to_typed(error: ProcessError) -> AnthropicError {
    match error {
        ProcessError::NotPermitted | ProcessError::BinaryNotFound => AnthropicError::CliNotFound,
        ProcessError::SpawnFailed { detail } => AnthropicError::AdapterError(format!("spawn claude: {detail}")),
        ProcessError::WaitFailed { detail } => AnthropicError::AdapterError(format!("wait for claude: {detail}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{http_error_to_typed, process_error_to_typed, retry_after_millis, status_to_error};
    use crate::kinds::AnthropicError;
    use aether_http::{HttpError, HttpHeader};
    use aether_process::ProcessError;

    #[test]
    fn unauthorized_statuses_map_to_unauthorized() {
        assert_eq!(status_to_error(401, None, ""), AnthropicError::Unauthorized);
        assert_eq!(status_to_error(403, None, ""), AnthropicError::Unauthorized);
    }

    #[test]
    fn rate_limit_threads_retry_after() {
        assert_eq!(
            status_to_error(429, Some(1500), "slow down"),
            AnthropicError::RateLimited { retry_after_millis: Some(1500) }
        );
    }

    #[test]
    fn overloaded_status_maps_to_overloaded() {
        assert_eq!(status_to_error(529, None, ""), AnthropicError::Overloaded);
    }

    #[test]
    fn other_status_carries_adapter_error() {
        let AnthropicError::AdapterError(msg) = status_to_error(500, None, "internal error") else {
            panic!("expected AdapterError");
        };
        assert!(msg.contains("500"));
        assert!(msg.contains("internal error"));
    }

    #[test]
    fn retry_after_header_seconds_to_millis() {
        let headers = vec![HttpHeader { name: "Retry-After".to_string(), value: "2".to_string() }];
        assert_eq!(retry_after_millis(&headers), Some(2000));
        assert_eq!(retry_after_millis(&[]), None);
    }

    #[test]
    fn http_timeout_maps_to_timeout() {
        assert_eq!(http_error_to_typed(HttpError::Timeout), AnthropicError::Timeout { elapsed_millis: 0 });
    }

    #[test]
    fn allowlist_denied_carries_adapter_error() {
        let AnthropicError::AdapterError(msg) = http_error_to_typed(HttpError::AllowlistDenied) else {
            panic!("expected AdapterError");
        };
        assert!(msg.contains("not permitted"));
    }

    #[test]
    fn process_refusal_and_missing_binary_map_to_cli_not_found() {
        assert_eq!(process_error_to_typed(ProcessError::NotPermitted), AnthropicError::CliNotFound);
        assert_eq!(process_error_to_typed(ProcessError::BinaryNotFound), AnthropicError::CliNotFound);
    }

    #[test]
    fn spawn_failure_preserves_detail() {
        let AnthropicError::AdapterError(msg) =
            process_error_to_typed(ProcessError::SpawnFailed { detail: "permission denied".to_string() })
        else {
            panic!("expected AdapterError");
        };
        assert!(msg.contains("permission denied"));
    }
}
