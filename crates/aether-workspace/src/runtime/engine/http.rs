//! HTTP/1.1 request writing and response parsing, over any byte stream.
//!
//! Every request carries `Connection: close`, so one connection holds one
//! exchange and a body with neither `Content-Length` nor chunked framing runs
//! to the end of the stream. Every line the client reads is bounded, and a
//! body is only ever read a buffer at a time, so no claimed length sizes an
//! allocation.

use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};

use super::EngineError;

/// The longest status, header, or chunk-size line accepted.
const MAX_LINE_BYTES: u64 = 8 * 1024;

/// The most header lines one response may carry.
const MAX_HEADERS: usize = 128;

/// The most of an error response's body kept for its message.
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// A request method this client sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

/// One request: the target is the path and query, the body a JSON document.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub method: Method,
    pub target: &'a str,
    pub json: Option<&'a [u8]>,
}

impl Request<'_> {
    /// Write the request line, the headers, and the body.
    pub fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        let body = self.json.unwrap_or_default();
        let content_type = if self.json.is_some() {
            "Content-Type: application/json\r\n"
        } else {
            ""
        };
        write!(
            out,
            "{} {} HTTP/1.1\r\nHost: docker\r\nUser-Agent: aether-workspace\r\nConnection: close\r\n{content_type}Content-Length: {}\r\n\r\n",
            self.method.as_str(),
            self.target,
            body.len()
        )?;
        out.write_all(body)?;
        out.flush()
    }
}

/// A response whose head has been read; its body is still on the wire.
pub struct Response<R> {
    pub status: u16,
    pub body: Body<R>,
}

impl<R: Read> Response<R> {
    /// Read a response head from `input`.
    ///
    /// # Errors
    ///
    /// [`EngineError::Protocol`] for a head that is not HTTP/1.x, a line past
    /// the bound, too many headers, or conflicting framing;
    /// [`EngineError::Io`] when reading fails.
    pub fn read(input: R) -> Result<Self, EngineError> {
        let mut reader = BufReader::new(input);
        let status_line =
            read_line(&mut reader)?.ok_or_else(|| protocol("the connection closed before a status line"))?;
        let status = parse_status(&status_line)?;

        let mut length = None;
        let mut chunked = false;
        for count in 0.. {
            let line = read_line(&mut reader)?.ok_or_else(|| protocol("the connection closed inside the headers"))?;
            if line.is_empty() {
                break;
            }
            if count == MAX_HEADERS {
                return Err(protocol("more than 128 header lines"));
            }
            let (name, value) = line.split_once(':').ok_or_else(|| protocol("a header line without a colon"))?;
            let value = value.trim();
            if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.rsplit(',').next().is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"));
                if !chunked {
                    return Err(protocol(&format!("unsupported transfer encoding {value:?}")));
                }
            } else if name.eq_ignore_ascii_case("content-length") {
                let parsed = value.parse::<u64>().map_err(|_| protocol("a malformed Content-Length"))?;
                if length.is_some_and(|earlier| earlier != parsed) {
                    return Err(protocol("conflicting Content-Length headers"));
                }
                length = Some(parsed);
            }
        }

        let framing = match (status, chunked, length) {
            (204 | 304, _, _) => Framing::Length(0),
            (_, true, Some(_)) => return Err(protocol("both Content-Length and chunked framing")),
            (_, true, None) => Framing::Chunked(Chunked::Between),
            (_, false, Some(length)) => Framing::Length(length),
            (_, false, None) => Framing::Close,
        };
        Ok(Self { status, body: Body { reader, framing } })
    }

    /// The body of a 2xx response.
    ///
    /// # Errors
    ///
    /// [`EngineError::Status`] for any other status, carrying the daemon's
    /// `message` when its bounded body is the Engine API's error JSON, or the
    /// bounded body text otherwise.
    pub fn success(self) -> Result<Body<R>, EngineError> {
        if (200..300).contains(&self.status) {
            return Ok(self.body);
        }
        let mut text = Vec::new();
        self.body.take(MAX_ERROR_BODY_BYTES).read_to_end(&mut text)?;
        let message = serde_json::from_slice::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("message").and_then(serde_json::Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&text).trim().to_owned());
        Err(EngineError::Status { status: self.status, message })
    }
}

/// A response body, read through its framing. Reading past the end yields
/// `Ok(0)`; a stream that ends early is an [`ErrorKind::UnexpectedEof`] error,
/// so a cut connection never reads as a shorter body.
pub struct Body<R> {
    reader: BufReader<R>,
    framing: Framing,
}

enum Framing {
    /// This many bytes are left.
    Length(u64),
    Chunked(Chunked),
    /// The body runs to the end of the stream.
    Close,
}

enum Chunked {
    /// At a chunk-size line.
    Between,
    /// Inside a chunk with this many bytes left.
    Inside(u64),
    /// The last chunk and the trailers have been read.
    Done,
}

impl<R: Read> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.framing {
                Framing::Close => return self.reader.read(buf),
                Framing::Length(0) | Framing::Chunked(Chunked::Done) => return Ok(0),
                Framing::Length(ref mut left) => {
                    let read = read_bounded(&mut self.reader, buf, *left)?;
                    *left -= read as u64;
                    return Ok(read);
                }
                Framing::Chunked(Chunked::Inside(left)) => {
                    let read = read_bounded(&mut self.reader, buf, left)?;
                    let left = left - read as u64;
                    if left == 0 {
                        expect_crlf(&mut self.reader)?;
                        self.framing = Framing::Chunked(Chunked::Between);
                    } else {
                        self.framing = Framing::Chunked(Chunked::Inside(left));
                    }
                    return Ok(read);
                }
                Framing::Chunked(Chunked::Between) => {
                    self.framing = match read_chunk_size(&mut self.reader)? {
                        0 => {
                            skip_trailers(&mut self.reader)?;
                            Framing::Chunked(Chunked::Done)
                        }
                        size => Framing::Chunked(Chunked::Inside(size)),
                    };
                }
            }
        }
    }
}

/// Read at most `left` bytes into `buf`; zero bytes from a non-empty budget is
/// a truncated body.
fn read_bounded(reader: &mut impl Read, buf: &mut [u8], left: u64) -> io::Result<usize> {
    let cap = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
    let read = reader.read(&mut buf[..cap])?;
    if read == 0 {
        return Err(io::Error::new(ErrorKind::UnexpectedEof, "the response body ended early"));
    }
    Ok(read)
}

fn read_chunk_size(reader: &mut impl BufRead) -> io::Result<u64> {
    let line = read_line(reader)
        .map_err(to_io)?
        .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "the response body ended before a chunk size"))?;
    let digits = line.split(';').next().unwrap_or_default().trim();
    u64::from_str_radix(digits, 16)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, format!("a malformed chunk size {digits:?}")))
}

fn expect_crlf(reader: &mut impl BufRead) -> io::Result<()> {
    match read_line(reader).map_err(to_io)? {
        Some(line) if line.is_empty() => Ok(()),
        Some(_) => Err(io::Error::new(ErrorKind::InvalidData, "a chunk longer than its size")),
        None => Err(io::Error::new(ErrorKind::UnexpectedEof, "the response body ended inside a chunk")),
    }
}

fn skip_trailers(reader: &mut impl BufRead) -> io::Result<()> {
    for _ in 0..=MAX_HEADERS {
        match read_line(reader).map_err(to_io)? {
            Some(line) if line.is_empty() => return Ok(()),
            Some(_) => {}
            None => return Err(io::Error::new(ErrorKind::UnexpectedEof, "the response ended inside its trailers")),
        }
    }
    Err(io::Error::new(ErrorKind::InvalidData, "more than 128 trailer lines"))
}

/// Read one CRLF- or LF-terminated line of at most [`MAX_LINE_BYTES`], without
/// its terminator. `None` at a clean end of stream.
fn read_line(reader: &mut impl BufRead) -> Result<Option<String>, EngineError> {
    let mut line = Vec::new();
    let read = reader.take(MAX_LINE_BYTES + 1).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        return Err(if read as u64 > MAX_LINE_BYTES {
            protocol("a line longer than 8 KiB")
        } else {
            EngineError::Io(io::Error::new(ErrorKind::UnexpectedEof, "the connection closed inside a line"))
        });
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map(Some).map_err(|_| protocol("a line that is not UTF-8"))
}

fn parse_status(line: &str) -> Result<u16, EngineError> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(protocol(&format!("a status line that is not HTTP/1.x: {line:?}")));
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|code| (100..600).contains(code))
        .ok_or_else(|| protocol(&format!("a malformed status line {line:?}")))
}

fn protocol(detail: &str) -> EngineError {
    EngineError::Protocol(detail.to_owned())
}

/// Carry a line-reading failure through `Read`, which speaks `io::Error`.
fn to_io(error: EngineError) -> io::Error {
    match error {
        EngineError::Io(error) => error,
        other => io::Error::new(ErrorKind::InvalidData, other.to_string()),
    }
}
