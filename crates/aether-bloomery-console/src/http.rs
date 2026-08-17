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
    if status >= 400 {
        let detail = serde_json::from_slice::<ErrorBody>(&bytes)
            .ok()
            .map_or_else(|| String::from_utf8_lossy(&bytes).into_owned(), |body| body.error);
        bail!("GET {path} failed ({status}): {detail}");
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("GET {path} returned {status} but the body is not the expected JSON shape"))
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    error: String,
}

fn exchange(endpoint: &Endpoint, method: &str, path: &str, timeout: Duration) -> Result<(u16, Vec<u8>)> {
    let request = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", endpoint.host);
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
    use super::parse_response;

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
