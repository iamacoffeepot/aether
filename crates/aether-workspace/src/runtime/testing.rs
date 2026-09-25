//! Test support: a scripted Engine API server on a Unix socket, and a tar
//! writer that can spell what an image export holds (absolute symlinks and
//! device nodes) but the canonical encoder never writes.
//!
//! [`StubDaemon`] answers each connection with the next scripted reply, in
//! order, whatever the request, and hands back every request it read. It
//! serves on a scoped thread the test owns. A connection the script expects
//! but the client never makes ends the serve after [`ACCEPT_WAIT`], with the
//! requests read so far, so a skipped request fails its test's assertion
//! instead of hanging it; once [`StubDaemon::serve`] has spent its script it
//! drops its listener, so a request the script did not expect fails fast
//! too.

use std::io::{self, BufRead, BufReader, ErrorKind, Write};
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
        let mut requests = Vec::with_capacity(replies.len());
        for reply in replies {
            let Some(stream) = self.accept_within(ACCEPT_WAIT)? else {
                break;
            };
            let mut reader = BufReader::new(stream);
            requests.push(read_request(&mut reader)?);
            let mut stream = reader.into_inner();
            // Ignored: a client that has what it needs closes early.
            let _ = write_reply(&mut stream, &reply);
        }
        Ok(requests)
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
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().map_err(io::Error::other)?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(StubRequest { method, target, body })
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
    }
    out.flush()
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
