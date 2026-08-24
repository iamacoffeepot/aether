//! Minimal HTTP/1.1 JSON client over `TcpStream`.
//!
//! The console has no HTTP crate and only ever talks to localhost (or an
//! ssh port-forward that looks like one). The coordinator REST surface is
//! HTTP/1.1 JSON, the same shape `xtask/src/bloom/http.rs` drives.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::str;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;

/// One coordinator the console talks to.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    /// Bearer token for the coordinator's control routes, when one is
    /// configured. `None` — and an empty string, which is what an unset
    /// `AETHER_HTTP_CONTROL_TOKEN` expands to — sends no `Authorization`
    /// header at all, which is what every route outside `/commissions`
    /// expects and what a predating coordinator answers.
    pub token: Option<String>,
}

impl Endpoint {
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// `GET path` and decode the JSON body.
pub fn get_json<T: DeserializeOwned>(endpoint: &Endpoint, path: &str, timeout: Duration) -> Result<T> {
    let (status, bytes) = exchange(endpoint, "GET", path, timeout)?;
    decode_json(path, status, &bytes)
}

/// `GET path` that treats `404` as absence rather than a failed body.
///
/// A predating coordinator has no commission routes; the backlog probe
/// caches that fact and must not treat it as a malformed document.
pub fn get_json_optional<T: DeserializeOwned>(endpoint: &Endpoint, path: &str, timeout: Duration) -> Result<Option<T>> {
    let (status, bytes) = exchange(endpoint, "GET", path, timeout)?;
    if status == 404 {
        return Ok(None);
    }
    decode_json(path, status, &bytes).map(Some)
}

fn decode_json<T: DeserializeOwned>(path: &str, status: u16, bytes: &[u8]) -> Result<T> {
    if status >= 400 {
        let detail = serde_json::from_slice::<ErrorBody>(bytes)
            .ok()
            .map_or_else(|| String::from_utf8_lossy(bytes).into_owned(), |body| body.error);
        if status == 401 {
            bail!(
                "GET {path} failed (401): {detail}; this route needs the coordinator control token \
                 — pass --token or set AETHER_HTTP_CONTROL_TOKEN"
            );
        }
        bail!("GET {path} failed ({status}): {detail}");
    }
    serde_json::from_slice(bytes)
        .with_context(|| format!("GET {path} returned {status} but the body is not the expected JSON shape"))
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    error: String,
}

/// The request head the console puts on the wire.
///
/// The commission routes are bearer-gated; every other route ignores the
/// header, so sending it whenever a token is configured costs nothing and
/// keeps the caller from having to know which routes need it.
fn build_request(endpoint: &Endpoint, method: &str, path: &str) -> String {
    let authorization = endpoint
        .token
        .as_deref()
        .filter(|token| !token.is_empty())
        .map_or_else(String::new, |token| format!("Authorization: Bearer {token}\r\n"));
    format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{authorization}\r\n", endpoint.host)
}

fn exchange(endpoint: &Endpoint, method: &str, path: &str, timeout: Duration) -> Result<(u16, Vec<u8>)> {
    let request = build_request(endpoint, method, path);
    let addr = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .with_context(|| format!("resolve coordinator at {}", endpoint.label()))?
        .next()
        .ok_or_else(|| anyhow!("coordinator at {} has no addresses", endpoint.label()))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("connect to coordinator at {}", endpoint.label()))?;
    stream.set_read_timeout(Some(timeout)).context("set read timeout")?;
    stream.set_write_timeout(Some(timeout)).context("set write timeout")?;
    stream.write_all(request.as_bytes()).context("write request")?;
    stream.flush().context("flush request")?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).context("read response")?;
    parse_response(&response)
}

fn parse_response(response: &[u8]) -> Result<(u16, Vec<u8>)> {
    let separator = b"\r\n\r\n";
    let head_end = response
        .windows(separator.len())
        .position(|window| window == separator)
        .ok_or_else(|| anyhow!("coordinator response has no header terminator"))?;
    let head = str::from_utf8(&response[..head_end]).context("coordinator response head is not UTF-8")?;
    let status = head
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("coordinator status line has no code"))?
        .parse()
        .context("coordinator status code")?;
    Ok((status, response[head_end + separator.len()..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::{Endpoint, build_request, decode_json, parse_response};

    fn endpoint(token: Option<&str>) -> Endpoint {
        Endpoint { host: "127.0.0.1".to_owned(), port: 8910, token: token.map(ToOwned::to_owned) }
    }

    #[test]
    fn a_configured_token_rides_on_every_request() {
        // The plausible bug: the console sends no Authorization header, so
        // every bearer-gated commission route answers 401, the absence probe
        // never latches, and the backlog pane paints an error for the life of
        // the session while re-issuing the request at view cadence.
        let request = build_request(&endpoint(Some("s3cret")), "GET", "/commissions");
        assert!(request.contains("\r\nAuthorization: Bearer s3cret\r\n"), "{request}");
        assert!(request.ends_with("\r\n\r\n"), "the header block is unterminated: {request}");
    }

    #[test]
    fn an_absent_or_empty_token_sends_no_authorization_header() {
        // The plausible bug: an unconfigured token still emits the header, so
        // a coordinator predating the control gate sees `Bearer ` on an open
        // route. An unset AETHER_HTTP_CONTROL_TOKEN expands to the empty
        // string, so both spellings of "no token" must stay off the wire.
        for token in [None, Some("")] {
            let request = build_request(&endpoint(token), "GET", "/view");
            assert!(!request.contains("Authorization"), "{token:?} put a header on the wire: {request}");
            assert!(request.ends_with("\r\n\r\n"), "the header block is unterminated: {request}");
        }
    }

    #[test]
    fn a_401_names_the_remedy_and_other_failures_do_not() {
        // The plausible bug: the gated routes fail with a bare
        // "unauthenticated" that tells the operator nothing about the flag
        // that would fix it.
        let refused = decode_json::<serde_json::Value>("/commissions", 401, br#"{"error":"unauthenticated"}"#)
            .expect_err("401 is a failure");
        assert!(refused.to_string().contains("--token"), "{refused}");
        let broken = decode_json::<serde_json::Value>("/view", 500, br#"{"error":"boom"}"#).expect_err("500 fails");
        assert!(!broken.to_string().contains("--token"), "{broken}");
    }

    #[test]
    fn parse_response_splits_status_and_body() {
        // The plausible bug: the splitter keeps the header terminator in the
        // body, so serde fails on a well-formed /view and the board stays stale.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"blooms\":[]}";
        let (status, body) = parse_response(raw).expect("a well-formed response parses");
        assert_eq!(status, 200);
        assert_eq!(body, br#"{"blooms":[]}"#);
    }

    #[test]
    fn parse_response_refuses_a_headless_body() {
        // The plausible bug: a truncated read is treated as JSON and the
        // decode error names the body instead of the missing terminator.
        let err = parse_response(b"HTTP/1.1 200 OK\r\n").expect_err("no terminator");
        assert!(err.to_string().contains("header terminator"), "{err}");
    }
}
