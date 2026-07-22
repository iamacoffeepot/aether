//! Provider-agnostic, I/O-free string helpers the content-gen components
//! share: the `status=<n>` error-string prefix parse and the body-snippet
//! trim.
//!
//! These are the pure fraction of the shared transport plumbing (ADR-0159
//! §2 names them as guest-portable). They depend only on `alloc`/`std`
//! string handling — no `ureq`, no substrate — so they compile to `wasm32`
//! unchanged, which is what lets the `aether.gemini` guest component map a
//! non-2xx `aether.http.fetch_result` onto its error taxonomy.

/// Parse the `<status> retry_after_millis=<Debug-of-Option<u32>>` prefix a
/// backend prepends to a non-2xx error string (after the caller strips
/// the leading `status=`). Returns `(status, retry_after_millis)` on a
/// clean parse. Both providers format the prefix identically.
#[must_use]
pub fn parse_status_prefix(rest: &str) -> Option<(u16, Option<u32>)> {
    let mut parts = rest.split_whitespace();
    let status = parts.next()?.parse::<u16>().ok()?;
    let retry_after_millis = parts.next().and_then(|tok| {
        tok.strip_prefix("retry_after_millis=").and_then(|v| {
            // The backend formats `Option<u32>` via Debug — `Some(1500)`
            // or `None`. Extract the inner integer when present.
            v.strip_prefix("Some(").and_then(|s| s.strip_suffix(')')).and_then(|n| n.parse::<u32>().ok())
        })
    });
    Some((status, retry_after_millis))
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
        assert_eq!(parse_status_prefix("429 retry_after_millis=Some(1500) body=x"), Some((429, Some(1500))));
        assert_eq!(parse_status_prefix("500 retry_after_millis=None body=oops"), Some((500, None)));
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
