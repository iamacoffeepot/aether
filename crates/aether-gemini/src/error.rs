//! HTTP-status → [`GeminiError`] mapping for the `aether.gemini` component
//! (ADR-0050 §1). The guest calls [`status_to_error`] when the provider returns
//! a non-2xx status; per-model validation (in `nanobanana.rs` / `lyria.rs`)
//! builds the structured `*NotSupportedByModel` / `MissingRequiredField` /
//! `UnknownModel` variants directly.

use super::GeminiError;

/// Trim a response body to a short diagnostic snippet so an adapter error
/// message stays log-sized even when the provider returns a multi-kilobyte
/// error page. Truncates on a char boundary. I/O-free, so it compiles to
/// `wasm32` unchanged.
#[must_use]
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
    use super::{snippet, status_to_error};
    use crate::GeminiError;

    #[test]
    fn snippet_truncates_on_char_boundary() {
        let long = "x".repeat(1000);
        let s = snippet(&long);
        assert!(s.len() <= 260);
        assert!(s.ends_with('…'));
        assert_eq!(snippet("short"), "short");
    }

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
