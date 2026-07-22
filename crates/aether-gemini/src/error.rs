//! HTTP-status → [`GeminiError`] mapping for the `aether.gemini` component
//! (ADR-0050 §1). The guest calls [`status_to_error`] when the provider returns
//! a non-2xx status; per-model validation (in `nanobanana.rs` / `lyria.rs`)
//! builds the structured `*NotSupportedByModel` / `MissingRequiredField` /
//! `UnknownModel` variants directly.

use super::GeminiError;

use aether_contentgen::snippet;

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

#[cfg(test)]
mod tests {
    use super::status_to_error;
    use crate::GeminiError;

    #[test]
    fn unauthorized_statuses_map_to_unauthorized() {
        assert_eq!(status_to_error(401, None, ""), GeminiError::Unauthorized);
        assert_eq!(status_to_error(403, None, ""), GeminiError::Unauthorized);
    }

    #[test]
    fn rate_limit_threads_retry_after() {
        assert_eq!(status_to_error(429, Some(2000), ""), GeminiError::RateLimited { retry_after_millis: Some(2000) });
    }

    #[test]
    fn unrecognised_error_is_adapter_error() {
        let err = status_to_error(500, None, "internal");
        assert!(matches!(err, GeminiError::AdapterError(_)));
    }
}
