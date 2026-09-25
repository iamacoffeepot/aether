//! Test support: a scripted Engine API server on a Unix socket, and a tar
//! writer that can spell what an image export holds (absolute symlinks and
//! device nodes) but the canonical encoder never writes.
//!
//! [`StubDaemon`] answers each connection with the next scripted reply, in
//! order, whatever the request, and hands back every request it read, a
//! chunked request body decoded. It serves on a scoped thread the test owns.
//! A reply can also hang up unanswered, hold a response open until the client
//! gives up, or take a hijacked connection over and record what the client
//! streams into it. A connection the script expects
//! but the client never makes ends the serve after [`ACCEPT_WAIT`], with the
//! requests read so far, so a skipped request fails its test's assertion
//! instead of hanging it; once [`StubDaemon::serve`] has spent its script it
//! drops its listener, so a request the script did not expect fails fast
//! too.

use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;

/// How long the stub waits for each scripted connection before it stops.
pub const ACCEPT_WAIT: Duration = Duration::from_secs(10);

/// How often a waiting stub looks for a connection.
const ACCEPT_POLL: Duration = Duration::from_millis(5);

/// A Unix-socket Engine API stand-in, bound and ready to serve.
pub struct StubDaemon {
    /// Holds the socket; removed when the daemon is done.
    _dir: TempDir,
    socket: PathBuf,
    listener: UnixListener,
}

/// One request the stub read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubRequest {
    pub method: String,
    /// The path and query.
    pub target: String,
    pub body: Vec<u8>,
}

impl StubRequest {
    /// `"<METHOD> <target>"`, the shape tests compare request orders in.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{} {}", self.method, self.target)
    }
}

/// One scripted response.
#[derive(Debug, Clone)]
pub struct StubReply {
    status: u16,
    body: StubBody,
}

#[derive(Debug, Clone)]
enum StubBody {
    Length(Vec<u8>),
    Chunked(Vec<Vec<u8>>),
    /// Close the connection without answering.
    HangUp,
    /// Send a chunked head, then read until the client closes.
    Hold,
    /// Answer `101`, then record everything the client streams until it
    /// closes its writing half, as the request's body.
    Upgrade,
}

impl StubReply {
    /// A `Content-Length` body.
    #[must_use]
    pub fn with_length(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self { status, body: StubBody::Length(body.into()) }
    }

    /// A chunked body, one chunk per element; empty elements are skipped, so
    /// the terminating chunk is always the zero-length one the stub adds.
    #[must_use]
    pub fn chunked(status: u16, chunks: Vec<Vec<u8>>) -> Self {
        Self { status, body: StubBody::Chunked(chunks) }
    }

    /// Close the connection after reading the request, answering nothing: a
    /// transport failure mid-call.
    #[must_use]
    pub fn hang_up() -> Self {
        Self { status: 0, body: StubBody::HangUp }
    }

    /// A `200` head whose body never comes: the stub reads until the client
    /// closes, as a `wait` on a container that never stops.
    #[must_use]
    pub fn hold() -> Self {
        Self { status: 200, body: StubBody::Hold }
    }

    /// `101 UPGRADED`, then the connection is the client's raw stream; what
    /// it writes becomes the request's recorded body.
    #[must_use]
    pub fn upgrade() -> Self {
        Self { status: 101, body: StubBody::Upgrade }
    }

    /// The five replies a successful import of `image` reads, in order: a
    /// pull stream, an inspect listing `image`, a create answering `id`, the
    /// export `tar` in 64 KiB chunks, and the removal.
    #[must_use]
    pub fn import_script(image: &str, id: &str, tar: &[u8]) -> Vec<Self> {
        vec![
            Self::chunked(200, vec![br#"{"status":"Pulling"}"#.to_vec(), b"\r\n".to_vec()]),
            Self::with_length(200, format!(r#"{{"Id":"sha256:1","RepoDigests":["{image}"]}}"#)),
            Self::with_length(201, format!(r#"{{"Id":"{id}","Warnings":[]}}"#)),
            Self::chunked(200, tar.chunks(64 * 1024).map(<[u8]>::to_vec).collect()),
            Self::with_length(204, Vec::new()),
        ]
    }
}

impl StubDaemon {
    /// Bind a fresh socket in a new temp directory; the path stays well
    /// under the platform's socket path limit.
    ///
    /// # Errors
    ///
    /// When the directory or the socket cannot be created.
    pub fn bind() -> io::Result<Self> {
        let dir = tempfile::Builder::new().prefix("ws").tempdir()?;
        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket)?;
        Ok(Self { _dir: dir, socket, listener })
    }

    /// The `unix://` endpoint a client dials.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("unix://{}", self.socket.display())
    }

    /// Answer one connection per reply, in order, and return the requests
    /// read, then drop the listener. It stops early, returning what it read,
    /// when a scripted connection does not arrive within [`ACCEPT_WAIT`]. A write the client stops reading early is
    /// not an error: a tar decode stops at the first end-of-archive block.
    ///
    /// # Errors
    ///
    /// When accepting a connection or reading its request fails.
    pub fn serve(self, replies: Vec<StubReply>) -> io::Result<Vec<StubRequest>> {
        self.answer(replies)
    }

    /// [`StubDaemon::serve`], keeping the listener bound for a later script.
    ///
    /// # Errors
    ///
    /// As [`StubDaemon::serve`].
    pub fn answer(&self, replies: Vec<StubReply>) -> io::Result<Vec<StubRequest>> {
        thread::scope(|scope| {
            let mut requests = Vec::with_capacity(replies.len());
            let mut hijacked = Vec::new();
            for reply in replies {
                let Some(stream) = self.accept_within(ACCEPT_WAIT)? else {
                    break;
                };
                let mut reader = BufReader::new(stream);
                requests.push(read_request(&mut reader)?);
                let mut stream = reader.into_inner();
                match reply.body {
                    StubBody::HangUp => {}
                    StubBody::Hold => {
                        // Ignored: the client closing is what ends the hold.
                        let _ = stream.write_all(b"HTTP/1.1 200 Stub\r\nTransfer-Encoding: chunked\r\n\r\n");
                        let _ = io::copy(&mut stream, &mut io::sink());
                    }
                    StubBody::Upgrade => {
                        stream.write_all(b"HTTP/1.1 101 UPGRADED\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n")?;
                        let index = requests.len() - 1;
                        hijacked.push((index, scope.spawn(move || read_to_close(stream))));
                    }
                    StubBody::Length(_) | StubBody::Chunked(_) => {
                        // Ignored: a client that has what it needs closes early.
                        let _ = write_reply(&mut stream, &reply);
                    }
                }
            }
            for (index, streamed) in hijacked {
                requests[index].body = streamed.join().map_err(|_| io::Error::other("a hijack reader panicked"))??;
            }
            Ok(requests)
        })
    }

    /// The next connection, or `None` once `wait` passes without one.
    fn accept_within(&self, wait: Duration) -> io::Result<Option<UnixStream>> {
        let deadline = Instant::now() + wait;
        self.listener.set_nonblocking(true)?;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    return Ok(Some(stream));
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(ACCEPT_POLL);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error),
            }
        }
    }
}

fn read_request(reader: &mut impl BufRead) -> io::Result<StubRequest> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();

    let mut length = 0;
    let mut chunked = false;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().map_err(io::Error::other)?;
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.trim().eq_ignore_ascii_case("chunked");
            }
        }
    }
    let body = if chunked {
        read_chunked(reader)?
    } else {
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        body
    };
    Ok(StubRequest { method, target, body })
}

/// A chunked request body, decoded; a malformed one is an error.
fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let size = usize::from_str_radix(line.trim_end(), 16).map_err(io::Error::other)?;
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        line.clear();
        reader.read_line(&mut line)?;
        if line != "\r\n" {
            return Err(io::Error::other("a chunk not followed by CRLF"));
        }
        if size == 0 {
            return Ok(body);
        }
    }
}

/// Everything a hijacked client streams until it closes its writing half.
fn read_to_close(mut stream: UnixStream) -> io::Result<Vec<u8>> {
    let mut streamed = Vec::new();
    stream.read_to_end(&mut streamed)?;
    Ok(streamed)
}

fn write_reply(out: &mut impl Write, reply: &StubReply) -> io::Result<()> {
    write!(out, "HTTP/1.1 {} Stub\r\nConnection: close\r\n", reply.status)?;
    match &reply.body {
        StubBody::Length(body) => {
            write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
            out.write_all(body)?;
        }
        StubBody::Chunked(chunks) => {
            out.write_all(b"Transfer-Encoding: chunked\r\n\r\n")?;
            for chunk in chunks.iter().filter(|chunk| !chunk.is_empty()) {
                write!(out, "{:x};stub=1\r\n", chunk.len())?;
                out.write_all(chunk)?;
                out.write_all(b"\r\n")?;
            }
            out.write_all(b"0\r\n\r\n")?;
        }
        StubBody::HangUp | StubBody::Hold | StubBody::Upgrade => {}
    }
    out.flush()
}

/// A multiplexed log stream as the Engine API frames one: each
/// `(stream, payload)` is an 8-byte header — the stream byte (`1` stdout,
/// `2` stderr), three zeros, the big-endian length — then the payload.
#[must_use]
pub fn log_stream(frames: &[(u8, &[u8])]) -> Vec<u8> {
    let mut stream = Vec::new();
    for (kind, payload) in frames {
        stream.extend_from_slice(&[*kind, 0, 0, 0]);
        stream.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(u32::MAX).to_be_bytes());
        stream.extend_from_slice(payload);
    }
    stream
}

/// What one scripted single-step run's daemon answers.
pub struct RunScript<'a> {
    /// The environment digest, in hex, the image label carries.
    pub environment: &'a str,
    /// The step's log frames, as [`log_stream`] takes them.
    pub logs: &'a [(u8, &'a [u8])],
    /// `State.ExitCode`.
    pub exit_code: i64,
    /// The tar `GET …/archive?path=/work` answers.
    pub output: &'a [u8],
}

/// The container id every scripted run's step container gets.
pub const RUN_CONTAINER: &str = "c0ffee";

/// The name every scripted run's `/work` volume gets.
pub const RUN_VOLUME: &str = "v0lume";

impl RunScript<'_> {
    /// The fourteen replies a single-step run with no mounts and no stdin
    /// reads, in order, when the environment image is already present.
    #[must_use]
    pub fn replies(&self) -> Vec<StubReply> {
        let only = |kind: u8| -> Vec<(u8, &[u8])> {
            self.logs.iter().filter(|(stream, _)| *stream == kind).copied().collect()
        };
        vec![
            StubReply::with_length(200, r#"{"Architecture":"x86_64","OSType":"linux"}"#),
            StubReply::with_length(
                200,
                format!(r#"{{"Config":{{"Labels":{{"aether.workspace.environment":"{}"}}}}}}"#, self.environment),
            ),
            StubReply::with_length(201, format!(r#"{{"Name":"{RUN_VOLUME}"}}"#)),
            StubReply::with_length(201, format!(r#"{{"Id":"{RUN_CONTAINER}","Warnings":[]}}"#)),
            StubReply::with_length(200, Vec::new()),
            StubReply::with_length(204, Vec::new()),
            StubReply::chunked(200, vec![br#"{"StatusCode":0,"Error":null}"#.to_vec()]),
            StubReply::with_length(200, format!(r#"{{"State":{{"ExitCode":{},"OOMKilled":false}}}}"#, self.exit_code)),
            StubReply::chunked(200, vec![log_stream(self.logs)]),
            StubReply::chunked(200, vec![log_stream(&only(1))]),
            StubReply::chunked(200, vec![log_stream(&only(2))]),
            StubReply::chunked(200, self.output.chunks(64 * 1024).map(<[u8]>::to_vec).collect()),
            StubReply::with_length(204, Vec::new()),
            StubReply::with_length(204, Vec::new()),
        ]
    }
}

/// A ustar archive written entry by entry, as a daemon's export spells it.
#[derive(Default)]
pub struct TarWriter {
    bytes: Vec<u8>,
}

const BLOCK_BYTES: usize = 512;

impl TarWriter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A directory entry, `path` ending in `/`.
    #[must_use]
    pub fn directory(self, path: &str) -> Self {
        self.entry(path, b'5', 0o755, "", &[])
    }

    /// A regular file of mode 0644.
    #[must_use]
    pub fn file(self, path: &str, content: &[u8]) -> Self {
        self.entry(path, b'0', 0o644, "", content)
    }

    /// A symlink to `target`, verbatim.
    #[must_use]
    pub fn symlink(self, path: &str, target: &str) -> Self {
        self.entry(path, b'2', 0o777, target, &[])
    }

    /// A character device node.
    #[must_use]
    pub fn char_device(self, path: &str) -> Self {
        self.entry(path, b'3', 0o666, "", &[])
    }

    /// A named pipe.
    #[must_use]
    pub fn fifo(self, path: &str) -> Self {
        self.entry(path, b'6', 0o644, "", &[])
    }

    /// The archive so far with its two end-of-archive blocks.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.bytes.resize(self.bytes.len() + 2 * BLOCK_BYTES, 0);
        self.bytes
    }

    /// The archive so far cut after `keep` bytes, as a dropped stream leaves it.
    #[must_use]
    pub fn cut(mut self, keep: usize) -> Vec<u8> {
        self.bytes.truncate(keep);
        self.bytes
    }

    fn entry(mut self, path: &str, typeflag: u8, mode: u32, link: &str, content: &[u8]) -> Self {
        let mut header = [0u8; BLOCK_BYTES];
        header[..path.len()].copy_from_slice(path.as_bytes());
        octal(&mut header[100..108], u64::from(mode));
        octal(&mut header[108..116], 0);
        octal(&mut header[116..124], 0);
        octal(&mut header[124..136], content.len() as u64);
        octal(&mut header[136..148], 0);
        header[156] = typeflag;
        header[157..157 + link.len()].copy_from_slice(link.as_bytes());
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].fill(b' ');
        let sum = header.iter().map(|&byte| u64::from(byte)).sum::<u64>();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());

        self.bytes.extend_from_slice(&header);
        self.bytes.extend_from_slice(content);
        let padding = content.len().next_multiple_of(BLOCK_BYTES) - content.len();
        self.bytes.resize(self.bytes.len() + padding, 0);
        self
    }
}

/// Write `value` as zero-padded octal filling all but the field's last byte,
/// which stays NUL.
fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    field[..digits].copy_from_slice(format!("{value:0digits$o}").as_bytes());
}

/// The artifact rows committed under the journal `root`, counted on a
/// read-only connection of the test's own.
///
/// # Errors
///
/// When the database cannot be opened or queried.
#[cfg(test)]
pub fn artifact_rows(root: &Path) -> Result<i64, rusqlite::Error> {
    let conn = Connection::open_with_flags(root.join("journal.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))
}
