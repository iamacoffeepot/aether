//! HTTP-status → [`GeminiError`] mapping for the `aether.gemini` cap
//! (ADR-0050 §1). The `ureq` backends call [`status_to_error`] when the
//! provider returns a non-2xx status; per-model validation (in
//! `nanobanana.rs` / `lyria.rs`) builds the structured
//! `*NotSupportedByModel` / `MissingRequiredField` / `UnknownModel`
//! variants directly.

use super::GeminiError;

use crate::shared::contentgen::shared::{parse_status_prefix, snippet};

/// Sentinel an adapter returns to mean "no API key" so the cap maps it
/// onto [`GeminiError::Unauthorized`]. The `DisabledGeminiAdapter`
/// returns this for every request.
pub const UNAUTHORIZED_SENTINEL: &str = "unauthorized";

/// Map an HTTP status code from a Gemini API onto a [`GeminiError`].
/// `retry_after_millis` is parsed from the `retry-after` header by the
/// caller; `body` is the response text, preserved in `AdapterError`
/// for the codes without a typed variant.
///
/// - `401` / `403` → `Unauthorized`
/// - `429` → `RateLimited`
/// - everything else non-2xx → `AdapterError` carrying status + snippet
#[must_use]
pub fn status_to_error(status: u16, retry_after_millis: Option<u32>, body: &str) -> GeminiError {
    match status {
        401 | 403 => GeminiError::Unauthorized,
        429 => GeminiError::RateLimited { retry_after_millis },
        other => GeminiError::AdapterError(format!("http {other}: {}", snippet(body))),
    }
}

/// Convert a free-form adapter error string into a typed
/// [`GeminiError`]. Recognises the `UNAUTHORIZED_SENTINEL` and the
/// `status=<n>` prefix the ureq backends prepend; falls back to
/// `AdapterError`.
#[must_use]
pub fn adapter_error_to_typed(raw: &str) -> GeminiError {
    if raw == UNAUTHORIZED_SENTINEL {
        return GeminiError::Unauthorized;
    }
    if let Some(rest) = raw.strip_prefix("status=")
        && let Some((status, retry_after_millis)) = parse_status_prefix(rest)
    {
        return status_to_error(status, retry_after_millis, rest);
    }
    GeminiError::AdapterError(snippet(raw))
}

#[cfg(test)]
mod tests {
    use super::{adapter_error_to_typed, status_to_error};
    use crate::gemini::GeminiError;

    #[test]
    fn unauthorized_statuses_map_to_unauthorized() {
        assert_eq!(status_to_error(401, None, ""), GeminiError::Unauthorized);
        assert_eq!(status_to_error(403, None, ""), GeminiError::Unauthorized);
    }

    #[test]
    fn rate_limit_threads_retry_after() {
        assert_eq!(
            status_to_error(429, Some(2000), ""),
            GeminiError::RateLimited {
                retry_after_millis: Some(2000)
            }
        );
    }

    #[test]
    fn unauthorized_sentinel_maps_to_unauthorized() {
        assert_eq!(
            adapter_error_to_typed(super::UNAUTHORIZED_SENTINEL),
            GeminiError::Unauthorized
        );
    }

    #[test]
    fn status_prefix_round_trips_through_typed() {
        let raw = "status=429 retry_after_millis=Some(1500) body=slow down";
        assert_eq!(
            adapter_error_to_typed(raw),
            GeminiError::RateLimited {
                retry_after_millis: Some(1500)
            }
        );
    }

    #[test]
    fn unrecognised_error_is_adapter_error() {
        let err = adapter_error_to_typed("connection refused");
        assert!(matches!(err, GeminiError::AdapterError(_)));
    }
}
