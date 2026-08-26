//! HTTP/1.1 client for the coordinator's REST control API.
//!
//! Talks only over localhost HTTP. Never opens the journal database.

use aether_bloomery::{HTTP_READ_TIMEOUT, http_success};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::str;

use crate::api::hex;

/// One control-API session against a running coordinator.
pub(super) struct ControlApi {
    pub(super) port: u16,
    pub(super) token: String,
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    error: String,
}

impl ControlApi {
    pub(super) fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.json("GET", path, None::<&()>)
    }

    /// GET that treats HTTP 404 as `None` rather than an error string to parse.
    pub(super) fn get_json_or_not_found<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let (status, response) = self.exchange("GET", path, None)?;
        if status == 404 {
            return Ok(None);
        }
        if !http_success(status) {
            bail!("{}", refuse(status, &response));
        }
        hex::from_slice(&response)
            .with_context(|| format!("decode GET {path} reply: {}", String::from_utf8_lossy(&response)))
    }

    pub(super) fn send_json<T: DeserializeOwned>(&self, method: &str, path: &str, body: &impl Serialize) -> Result<T> {
        self.json(method, path, Some(body))
    }

    fn json<T: DeserializeOwned>(&self, method: &str, path: &str, body: Option<&impl Serialize>) -> Result<T> {
        let encoded = match body {
            Some(value) => Some(hex::to_vec(value).context("encode request body")?),
            None => None,
        };
        let (status, response) = self.exchange(method, path, encoded.as_deref())?;
        if !http_success(status) {
            bail!("{}", refuse(status, &response));
        }
        hex::from_slice(&response)
            .with_context(|| format!("decode {method} {path} reply: {}", String::from_utf8_lossy(&response)))
    }

    fn exchange(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>)> {
        let auth = if self.token.is_empty() {
            String::new()
        } else {
            format!("Authorization: Bearer {}\r\n", self.token)
        };
        let content = body.map_or_else(String::new, |bytes| {
            format!("Content-Type: application/json\r\nContent-Length: {}\r\n", bytes.len())
        });
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{auth}{content}\r\n")
                .into_bytes();
        if let Some(bytes) = body {
            request.extend_from_slice(bytes);
        }

        let mut stream = TcpStream::connect(("127.0.0.1", self.port))
            .with_context(|| format!("connect to control API on 127.0.0.1:{}", self.port))?;
        stream.set_read_timeout(Some(HTTP_READ_TIMEOUT)).context("set control API read timeout")?;
        stream.write_all(&request).context("write control API request")?;
        stream.flush().context("flush control API request")?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).context("read control API response")?;
        parse_response(&response)
    }
}

fn refuse(status: u16, body: &[u8]) -> anyhow::Error {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(view) => anyhow!("{status}: {}", view.error),
        Err(_) => anyhow!("{status}: {}", String::from_utf8_lossy(body)),
    }
}

fn parse_response(response: &[u8]) -> Result<(u16, Vec<u8>)> {
    let separator = b"\r\n\r\n";
    let Some(head_end) = response.windows(separator.len()).position(|window| window == separator) else {
        bail!("control API response has no header terminator");
    };
    let head = &response[..head_end];
    let body = response[head_end + separator.len()..].to_vec();
    let status_line = head.split(|&byte| byte == b'\r').next().unwrap_or(head);
    let status_text = str::from_utf8(status_line).context("control API status line is not UTF-8")?;
    let Some(code) = status_text.split_whitespace().nth(1) else {
        bail!("control API status line has no code: {status_text}");
    };
    let status: u16 = code.parse().with_context(|| format!("control API status code {code}"))?;
    Ok((status, body))
}
