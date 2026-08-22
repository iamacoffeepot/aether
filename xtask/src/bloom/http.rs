//! Minimal HTTP/1.1 JSON client over `TcpStream`.
//!
//! xtask has no HTTP crate and this dispatch cannot add one. The coordinator
//! REST surface is localhost HTTP/1.1 JSON, the same shape
//! `crates/aether-http/tests/` drives with a raw socket.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::str;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::Endpoint;

const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// One JSON request. `body` is `None` for no-body methods (GET, empty POST).
pub fn json<T: DeserializeOwned>(
    endpoint: &Endpoint,
    method: &str,
    path: &str,
    body: Option<&impl Serialize>,
) -> Result<T> {
    let encoded = body.map(serde_json::to_vec).transpose().context("encode request body")?;
    let (status, bytes) = exchange(endpoint, method, path, encoded.as_deref())?;
    if status >= 400 {
        let detail = serde_json::from_slice::<ErrorBody>(&bytes)
            .ok()
            .map_or_else(|| String::from_utf8_lossy(&bytes).into_owned(), |body| body.error);
        bail!("{method} {path} failed ({status}): {detail}");
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("{method} {path} returned {status} but the body is not the expected JSON shape"))
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    error: String,
}

fn exchange(endpoint: &Endpoint, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>)> {
    // The commission routes are bearer-gated; every other route ignores the
    // header, so sending it whenever a token is configured costs nothing and
    // keeps the caller from having to know which routes need it.
    let authorization =
        endpoint.token.as_deref().map_or_else(String::new, |token| format!("Authorization: Bearer {token}\r\n"));
    let header = body.map_or_else(
        || format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{authorization}\r\n", endpoint.host),
        |bytes| {
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                endpoint.host,
                bytes.len()
            )
        },
    );
    let mut request = header.into_bytes();
    if let Some(bytes) = body {
        request.extend_from_slice(bytes);
    }

    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .with_context(|| format!("connect to coordinator at {}:{}", endpoint.host, endpoint.port))?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).context("set read timeout")?;
    stream.write_all(&request).context("write request")?;
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
    use super::parse_response;

    #[test]
    fn parse_response_splits_status_and_body() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}";
        let (status, body) = parse_response(raw).expect("a well-formed response parses");
        assert_eq!(status, 201);
        assert_eq!(body, br#"{"ok":true}"#);
    }
}
