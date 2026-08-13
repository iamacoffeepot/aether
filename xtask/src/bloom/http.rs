//! Minimal HTTP/1.1 JSON client over `std::net::TcpStream`.
//!
//! The coordinator REST surface is localhost JSON; xtask has no HTTP
//! client crate, so this speaks the same `Connection: close` dialect the
//! `aether-http` tests drive the server with.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::str;
use std::time::Duration;

use anyhow::{Context, Result, bail};

const HOST: &str = "127.0.0.1";
const READ_TIMEOUT: Duration = Duration::from_mins(1);

/// One HTTP response: status code and raw body bytes.
pub(super) struct Response {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
}

/// Issue `method path` on `port`, optionally with a JSON body.
pub(super) fn request(port: u16, method: &str, path: &str, body: Option<&[u8]>) -> Result<Response> {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n");
    if let Some(bytes) = body {
        let _ = write!(head, "Content-Type: application/json\r\nContent-Length: {}\r\n", bytes.len());
    }
    head.push_str("\r\n");

    let mut request = head.into_bytes();
    if let Some(bytes) = body {
        request.extend_from_slice(bytes);
    }

    let mut stream = TcpStream::connect((HOST, port)).with_context(|| format!("connect {HOST}:{port}"))?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).context("set read timeout")?;
    stream.write_all(&request).context("write request")?;
    stream.flush().context("flush request")?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).context("read response")?;
    parse_response(&response)
}

/// Split an HTTP response into its status code and body bytes.
fn parse_response(response: &[u8]) -> Result<Response> {
    let separator = b"\r\n\r\n";
    let head_end = response
        .windows(separator.len())
        .position(|window| window == separator)
        .context("HTTP response is missing a head/body separator")?;
    let head = &response[..head_end];
    let body = response[head_end + separator.len()..].to_vec();
    let status_line = head.split(|&byte| byte == b'\r').next().context("HTTP response is missing a status line")?;
    let status = str::from_utf8(status_line)
        .context("HTTP status line is not UTF-8")?
        .split_whitespace()
        .nth(1)
        .context("HTTP status line has no status code")?
        .parse()
        .context("HTTP status code is not a number")?;
    Ok(Response { status, body })
}

/// A 2xx JSON body, or the coordinator's error text.
pub(super) fn json(port: u16, method: &str, path: &str, body: Option<&[u8]>) -> Result<serde_json::Value> {
    let response = request(port, method, path, body)?;
    let parsed = if response.body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&response.body).with_context(|| {
            format!("{method} {path} returned non-JSON ({})", String::from_utf8_lossy(&response.body))
        })?
    };
    if !(200..300).contains(&response.status) {
        let message = parsed.get("error").and_then(serde_json::Value::as_str).unwrap_or("request failed");
        bail!("{method} {path} returned {}: {message}", response.status);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::parse_response;

    #[test]
    fn parse_response_splits_status_and_body() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}";
        let parsed = parse_response(raw).expect("a well-formed response parses");
        assert_eq!(parsed.status, 201);
        assert_eq!(parsed.body, br#"{"ok":true}"#);
    }
}
