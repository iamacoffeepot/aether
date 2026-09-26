//! HTTP/1.1 request writing and response parsing, over any byte stream.
//!
//! Every request but a hijack carries `Connection: close`, so one connection
//! holds one exchange and a body with neither `Content-Length` nor chunked
//! framing runs to the end of the stream. A request body is either a small
//! JSON document sent with its length, or a stream the caller writes through
//! [`ChunkedWriter`] one bounded chunk at a time, so a tar of any size crosses
//! without being held. Response heads and chunk-size lines parse through
//! [`httparse`] over the crate's own bounded line reads; every other part —
//! request writing, [`ChunkedWriter`], framing, and the bounds themselves —
//! is this module's own. A response body is only ever read a buffer at a
//! time, so no claimed length sizes an allocation.

use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::str::from_utf8;

use super::EngineError;

/// The longest status, header, or chunk-size line accepted.
const MAX_LINE_BYTES: u64 = 8 * 1024;

/// The most header lines one response may carry.
const MAX_HEADERS: usize = 128;

/// The most of an error response's body kept for its message.
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// The largest chunk [`ChunkedWriter`] sends.
const CHUNK_BYTES: usize = 64 * 1024;

/// A request method this client sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// What follows a request's head.
#[derive(Debug, Clone, Copy)]
pub enum RequestBody<'a> {
    /// Nothing: `Content-Length: 0`.
    Empty,
    /// A JSON document, sent with its length.
    Json(&'a [u8]),
    /// A chunked body of this content type, which the caller streams through
    /// a [`ChunkedWriter`] after the head.
    Chunked(&'static str),
    /// Nothing, and a request that the daemon hand the connection over as a
    /// raw stream once it answers (`Connection: Upgrade`).
    Upgrade,
}

/// One request: the target is the path and query.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub method: Method,
    pub target: &'a str,
    pub body: RequestBody<'a>,
}

impl Request<'_> {
    /// Write the request line and the headers, then the body when it is JSON.
    /// A chunked body is the caller's to write next.
    pub fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        write!(
            out,
            "{} {} HTTP/1.1\r\nHost: docker\r\nUser-Agent: aether-workspace\r\n",
            self.method.as_str(),
            self.target
        )?;
        match self.body {
            RequestBody::Empty => out.write_all(b"Connection: close\r\nContent-Length: 0\r\n\r\n")?,
            RequestBody::Json(json) => {
                write!(
                    out,
                    "Connection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    json.len()
                )?;
                out.write_all(json)?;
            }
            RequestBody::Chunked(content_type) => {
                write!(out, "Connection: close\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n")?;
            }
            RequestBody::Upgrade => {
                out.write_all(b"Connection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 0\r\n\r\n")?;
            }
        }
        out.flush()
    }
}

/// A chunked request body: bytes written to it leave in chunks of at most
/// [`CHUNK_BYTES`], and [`ChunkedWriter::finish`] sends the terminating chunk.
/// Dropping it unfinished leaves the body unterminated, so the daemon never
/// takes a cut stream as whole.
pub struct ChunkedWriter<W: Write> {
    out: W,
    buffer: Vec<u8>,
    failed: bool,
}

impl<W: Write> ChunkedWriter<W> {
    pub fn new(out: W) -> Self {
        Self { out, buffer: Vec::with_capacity(CHUNK_BYTES), failed: false }
    }

    /// Whether sending to the connection has failed, as opposed to the
    /// caller's own source.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Send what is buffered, then the zero-length chunk that ends the body.
    pub fn finish(mut self) -> io::Result<W> {
        self.send_chunk()?;
        self.out.write_all(b"0\r\n\r\n").and_then(|()| self.out.flush())?;
        Ok(self.out)
    }

    fn send_chunk(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let sent = write!(self.out, "{:x}\r\n", self.buffer.len())
            .and_then(|()| self.out.write_all(&self.buffer))
            .and_then(|()| self.out.write_all(b"\r\n"));
        self.failed |= sent.is_err();
        self.buffer.clear();
        sent
    }
}

impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let taken = bytes.len().min(CHUNK_BYTES - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..taken]);
        if self.buffer.len() == CHUNK_BYTES {
            self.send_chunk()?;
        }
        Ok(taken)
    }

    /// Sends what is buffered as a chunk; never the terminating one.
    fn flush(&mut self) -> io::Result<()> {
        self.send_chunk()?;
        let flushed = self.out.flush();
        self.failed |= flushed.is_err();
        flushed
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

        // Accumulate the status line and header lines into one buffer, up to
        // and including the blank line that ends the head — the line
        // terminators stay in, since `httparse` parses them as part of the
        // head. Bounded at `MAX_HEADERS` header lines before parsing ever
        // runs, so a peer cannot grow this buffer unboundedly.
        let mut head = read_line(&mut reader)?.ok_or_else(|| protocol("the connection closed before a status line"))?;
        let mut header_lines = 0usize;
        loop {
            let line = read_line(&mut reader)?.ok_or_else(|| protocol("the connection closed inside the headers"))?;
            let blank = is_blank(&line);
            head.extend_from_slice(&line);
            if blank {
                break;
            }
            if header_lines == MAX_HEADERS {
                return Err(protocol("more than 128 header lines"));
            }
            header_lines += 1;
        }

        let mut header_storage = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut response = httparse::Response::new(&mut header_storage);
        match response.parse(&head) {
            Ok(httparse::Status::Complete(len)) if len == head.len() => {}
            Ok(httparse::Status::Complete(_) | httparse::Status::Partial) => {
                return Err(protocol("a malformed response head"));
            }
            Err(httparse::Error::TooManyHeaders) => return Err(protocol("more than 128 header lines")),
            Err(httparse::Error::Version) => return Err(protocol("not HTTP/1.x")),
            Err(error) => return Err(protocol(&format!("a malformed response head: {error}"))),
        }

        // httparse accepts any three digits as a status code, so the
        // documented range still gets its own check.
        let status = response
            .code
            .filter(|code| (100..600).contains(code))
            .ok_or_else(|| protocol(&format!("a malformed status line {:?}", String::from_utf8_lossy(&head))))?;

        let mut length = None;
        let mut chunked = false;
        for header in response.headers.iter() {
            if header.name.eq_ignore_ascii_case("transfer-encoding") {
                let value = from_utf8(header.value).map_err(|_| protocol("unsupported transfer encoding"))?;
                chunked = value.rsplit(',').next().is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"));
                if !chunked {
                    return Err(protocol(&format!("unsupported transfer encoding {value:?}")));
                }
            } else if header.name.eq_ignore_ascii_case("content-length") {
                let value = from_utf8(header.value).map_err(|_| protocol("a malformed Content-Length"))?;
                let parsed = value.trim().parse::<u64>().map_err(|_| protocol("a malformed Content-Length"))?;
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

impl<R> Body<R> {
    /// The stream under the body, for a connection the daemon has hijacked.
    /// Bytes already buffered past the head are dropped, which is safe only
    /// where the daemon sends nothing after it, as a stdin-only attach does.
    pub fn into_stream(self) -> R {
        self.reader.into_inner()
    }
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

/// A chunk-size line's terminator is normalized to CRLF before it reaches
/// [`httparse::parse_chunk_size`] (which requires one), so the reader's own
/// tolerance for a bare LF survives the move to it.
fn read_chunk_size(reader: &mut impl BufRead) -> io::Result<u64> {
    let line = read_line(reader)
        .map_err(to_io)?
        .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "the response body ended before a chunk size"))?;
    let malformed = |line: &[u8]| {
        io::Error::new(ErrorKind::InvalidData, format!("a malformed chunk size {:?}", String::from_utf8_lossy(line)))
    };
    let stripped = strip_terminator(&line);
    // `httparse` reads a line with no hex digit at all — a bare terminator —
    // as chunk size zero, which would end the body early on a garbled
    // stream; refuse it before that ever runs.
    if !stripped.first().is_some_and(u8::is_ascii_hexdigit) {
        return Err(malformed(stripped));
    }
    let mut normalized = stripped.to_vec();
    normalized.extend_from_slice(b"\r\n");
    match httparse::parse_chunk_size(&normalized) {
        Ok(httparse::Status::Complete((_, size))) => Ok(size),
        Ok(httparse::Status::Partial) | Err(httparse::InvalidChunkSize) => Err(malformed(stripped)),
    }
}

fn expect_crlf(reader: &mut impl BufRead) -> io::Result<()> {
    match read_line(reader).map_err(to_io)? {
        Some(line) if is_blank(&line) => Ok(()),
        Some(_) => Err(io::Error::new(ErrorKind::InvalidData, "a chunk longer than its size")),
        None => Err(io::Error::new(ErrorKind::UnexpectedEof, "the response body ended inside a chunk")),
    }
}

fn skip_trailers(reader: &mut impl BufRead) -> io::Result<()> {
    for _ in 0..=MAX_HEADERS {
        match read_line(reader).map_err(to_io)? {
            Some(line) if is_blank(&line) => return Ok(()),
            Some(_) => {}
            None => return Err(io::Error::new(ErrorKind::UnexpectedEof, "the response ended inside its trailers")),
        }
    }
    Err(io::Error::new(ErrorKind::InvalidData, "more than 128 trailer lines"))
}

/// Read one CRLF- or LF-terminated line of at most [`MAX_LINE_BYTES`],
/// keeping its terminator — the head [`Response::read`] accumulates is
/// parsed by `httparse` as raw bytes, terminators included. `None` at a
/// clean end of stream.
fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, EngineError> {
    let mut line = Vec::new();
    let read = reader.take(MAX_LINE_BYTES + 1).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(if read as u64 > MAX_LINE_BYTES {
            protocol("a line longer than 8 KiB")
        } else {
            EngineError::Io(io::Error::new(ErrorKind::UnexpectedEof, "the connection closed inside a line"))
        });
    }
    Ok(Some(line))
}

/// Strip a line's terminator (`\r\n`, or a bare `\n`), for the callers that
/// read a [`read_line`] result as content rather than raw head bytes.
fn strip_terminator(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r\n".as_slice()).or_else(|| line.strip_suffix(b"\n".as_slice())).unwrap_or(line)
}

/// Whether a [`read_line`] result carries no content: a lone `\r\n` or `\n`.
fn is_blank(line: &[u8]) -> bool {
    strip_terminator(line).is_empty()
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
