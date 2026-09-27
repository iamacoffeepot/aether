//! A container's log stream, demultiplexed.
//!
//! Without a TTY the Engine API frames `GET /containers/{id}/logs` as a run of
//! frames, each an 8-byte header — the stream (`1` stdout, `2` stderr), three
//! zero bytes, and the payload length as a big-endian `u32` — then the
//! payload. [`count`] reads a whole stream and returns each output's length;
//! [`Demux`] yields one output's bytes and skips the other's. Neither holds
//! more than one copy buffer, whatever the log's size.

use std::io::{self, ErrorKind, Read};

/// The frame header's length.
const HEADER_BYTES: usize = 8;

/// The copy buffer [`count`] and [`Demux`] skip through.
const SKIP_BUFFER_BYTES: usize = 64 * 1024;

/// One of a step's two outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Stdout,
    Stderr,
}

impl Output {
    /// The stream byte a frame header names this output by.
    const fn stream_byte(self) -> u8 {
        match self {
            Self::Stdout => 1,
            Self::Stderr => 2,
        }
    }
}

/// The payload length of each output in one whole log stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lengths {
    pub stdout: u64,
    pub stderr: u64,
}

impl Lengths {
    /// The length of `output`.
    #[must_use]
    pub const fn of(self, output: Output) -> u64 {
        match output {
            Output::Stdout => self.stdout,
            Output::Stderr => self.stderr,
        }
    }
}

/// Read a whole log stream and return how many payload bytes each output
/// carried.
///
/// # Errors
///
/// As [`Demux`]'s reads: a cut frame, an unknown stream byte, or a daemon
/// error frame.
pub fn count(input: impl Read) -> io::Result<Lengths> {
    let mut frames = Frames { input, left: 0, stream: 0 };
    let mut lengths = Lengths::default();
    let mut buffer = vec![0; SKIP_BUFFER_BYTES];
    while let Some(stream) = frames.next_payload()? {
        let read = frames.read_payload(&mut buffer)?;
        let read = read as u64;
        if stream == Output::Stdout.stream_byte() {
            lengths.stdout += read;
        } else {
            lengths.stderr += read;
        }
    }
    Ok(lengths)
}

/// One output's bytes out of a log stream.
///
/// Reads end with `Ok(0)` only at a frame boundary; a stream cut inside a
/// header or a payload is an [`ErrorKind::UnexpectedEof`] error, so a dropped
/// connection never reads as a shorter log.
pub struct Demux<R> {
    frames: Frames<R>,
    wanted: u8,
    skip: Vec<u8>,
}

impl<R: Read> Demux<R> {
    pub fn new(input: R, wanted: Output) -> Self {
        Self { frames: Frames { input, left: 0, stream: 0 }, wanted: wanted.stream_byte(), skip: Vec::new() }
    }
}

impl<R: Read> Read for Demux<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(stream) = self.frames.next_payload()? else {
                return Ok(0);
            };
            if stream == self.wanted {
                return self.frames.read_payload(buf);
            }
            if self.skip.is_empty() {
                self.skip = vec![0; SKIP_BUFFER_BYTES];
            }
            self.frames.read_payload(&mut self.skip)?;
        }
    }
}

/// The frame walk both readers share.
struct Frames<R> {
    input: R,
    /// Payload bytes left in the current frame.
    left: u64,
    /// The current frame's stream byte.
    stream: u8,
}

impl<R: Read> Frames<R> {
    /// The stream byte of the frame whose payload is next to read, reading
    /// headers (and skipping empty frames) as needed; `None` at a clean end.
    fn next_payload(&mut self) -> io::Result<Option<u8>> {
        while self.left == 0 {
            let mut header = [0; HEADER_BYTES];
            if !self.header(&mut header)? {
                return Ok(None);
            }
            let [stream, _, _, _, size @ ..] = header;
            self.left = u64::from(u32::from_be_bytes(size));
            self.stream = stream;
            match stream {
                1 | 2 => {}
                3 => return Err(self.daemon_error()),
                other => {
                    return Err(io::Error::new(ErrorKind::InvalidData, format!("a log frame for stream {other}")));
                }
            }
        }
        Ok(Some(self.stream))
    }

    /// Read at most the current frame's remaining payload into `buf`.
    fn read_payload(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let cap = usize::try_from(self.left).unwrap_or(usize::MAX).min(buf.len());
        let read = self.input.read(&mut buf[..cap])?;
        if read == 0 {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "the log stream ended inside a frame"));
        }
        self.left -= read as u64;
        Ok(read)
    }

    /// Fill `header`; `false` when the stream ends before its first byte.
    fn header(&mut self, header: &mut [u8; HEADER_BYTES]) -> io::Result<bool> {
        let mut filled = 0;
        while filled < HEADER_BYTES {
            match self.input.read(&mut header[filled..]) {
                Ok(0) if filled == 0 => return Ok(false),
                Ok(0) => {
                    return Err(io::Error::new(ErrorKind::UnexpectedEof, "the log stream ended inside a frame header"));
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    /// A stream-3 frame carries the daemon's own error text; its payload,
    /// bounded, becomes the error.
    fn daemon_error(&mut self) -> io::Error {
        let mut text = Vec::new();
        let bound = self.left.min(SKIP_BUFFER_BYTES as u64);
        match (&mut self.input).take(bound).read_to_end(&mut text) {
            Ok(_) => io::Error::other(format!("the daemon failed the log stream: {}", String::from_utf8_lossy(&text))),
            Err(error) => error,
        }
    }
}
