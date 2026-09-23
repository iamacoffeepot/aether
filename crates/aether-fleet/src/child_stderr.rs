//! The forked substrate's stderr, teed to the hub log and kept as a tail.
//!
//! A substrate that dies during startup — a usage error at argv parse, a
//! boot error — explains itself on stderr and then exits. Inherited, that
//! explanation lands in the hub's own log, where neither the spawn reply
//! nor `list_engines.recently_died` can reach it. So the engines cap pipes
//! the child's stderr, and one reader per child drains the pipe until EOF:
//! each chunk goes straight on to the hub's stderr, as it would have with
//! an inherited descriptor, and the last [`STDERR_RING_BYTES`] stay in a
//! ring. At EOF the reader renders that ring once into a bounded,
//! path-free tail and hands it to the [`StderrTail`] the cap kept, which a
//! failed spawn collects into its `spawn_failed` detail.
//!
//! The reader never stops reading, even when the write to the hub's stderr
//! fails, so a live engine can never block on a full pipe this capture
//! created. Stdout stays inherited: usage errors go to stderr, and stdout
//! carries the `--describe` / `--print-config` payloads.
//!
//! Rendering answers ADR-0115: the realized executable path must not leave
//! the host, and neither may the fleet scratch root it sits under. The
//! child names its own path freely (`$0`, clap's usage line), so the tail
//! is redacted before it is ever handed out.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStderr};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

/// How much of the child's most recent stderr the reader keeps. Larger
/// than the detail cap so the redaction and the partial-line drop have
/// whole lines to work with before the final cut.
const STDERR_RING_BYTES: usize = 8 * 1024;

/// The most stderr a spawn-failure detail carries. Sixteen `spawn_failed`
/// rows — a full recently-died ring — then stay at or under the 32 KiB MCP
/// whole-response backstop.
const STDERR_DETAIL_MAX_BYTES: usize = 2 * 1024;

/// How long a failed spawn waits for the reader to reach EOF. The proxy's
/// failed init has already reaped the child's group, so EOF is normally
/// immediate; the grace covers a detached grandchild still holding the
/// pipe open, and on expiry the detail simply goes without a tail.
const STDERR_EOF_GRACE_MILLIS: u64 = 500;

/// Bytes the reader asks the pipe for per read.
const STDERR_READ_CHUNK_BYTES: usize = 4096;

/// Marks a tail whose front was cut off to fit [`STDERR_DETAIL_MAX_BYTES`].
const TRUNCATION_MARKER: char = '…';

/// Stand-in for the realized executable path in a rendered tail.
const SUBSTRATE_PLACEHOLDER: &str = "<substrate>";

/// Stand-in for the fleet scratch root in a rendered tail.
const FLEET_STORE_PLACEHOLDER: &str = "<fleet-store>";

/// The host paths one fork's stderr tail must not carry, each paired with
/// the placeholder that replaces it, longest needle first.
pub struct StderrRedactions {
    needles: Vec<(String, &'static str)>,
}

impl StderrRedactions {
    /// The redactions for one fork: the realized executable path and the
    /// fleet scratch root, each in its as-given and its canonical form.
    ///
    /// Longest first is what keeps a whole path whole. The executable
    /// sits under the root, so replacing the root first would leave
    /// `<fleet-store>/<id>/<app name>` and leak the application-name file
    /// name the full path carries. A path that will not canonicalize
    /// contributes its as-given form only.
    pub fn for_fork(exec_path: &Path, fleet_store_root: &Path) -> Self {
        let mut needles: Vec<(String, &'static str)> =
            [(exec_path, SUBSTRATE_PLACEHOLDER), (fleet_store_root, FLEET_STORE_PLACEHOLDER)]
                .into_iter()
                .flat_map(|(path, placeholder)| {
                    [Some(path.to_path_buf()), fs::canonicalize(path).ok()]
                        .into_iter()
                        .flatten()
                        .map(move |form| (form.to_string_lossy().into_owned(), placeholder))
                })
                .filter(|(needle, _)| !needle.is_empty())
                .collect();
        needles.sort_by(|(a, _), (b, _)| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        needles.dedup_by(|(a, _), (b, _)| a == b);
        Self { needles }
    }

    /// Replace every needle in `text` with its placeholder, in order.
    fn apply(&self, text: &str) -> String {
        self.needles.iter().fold(text.to_owned(), |text, (needle, placeholder)| text.replace(needle, placeholder))
    }
}

/// The receiving end of one child's rendered stderr tail.
pub struct StderrTail {
    receiver: Receiver<String>,
}

impl StderrTail {
    /// The rendered tail, once the child's stderr has reached EOF.
    ///
    /// `None` when the child wrote nothing worth reporting, or when EOF
    /// did not arrive within the grace period. Dropping the tail instead
    /// is always sound: the reader keeps teeing to the hub log, and its
    /// final send to a gone receiver is ignored.
    pub fn collect(self) -> Option<String> {
        self.receiver.recv_timeout(Duration::from_millis(STDERR_EOF_GRACE_MILLIS)).ok().filter(|tail| !tail.is_empty())
    }
}

/// The reader half of one child's stderr capture: the pipe, the
/// redactions its tail needs, and where the rendered tail goes.
///
/// The caller runs [`Self::run`] on a thread of its own; the tee does
/// not choose how it is spawned.
pub struct StderrTee {
    pipe: ChildStderr,
    redactions: StderrRedactions,
    sender: SyncSender<String>,
}

impl StderrTee {
    /// Drain the pipe to EOF, teeing every chunk to the hub's stderr and
    /// keeping the most recent bytes, then send the rendered tail once.
    ///
    /// EOF arrives when the child and everything it forked that still
    /// holds the pipe are gone. A read error ends the drain the same way:
    /// the pipe is unusable, and what was kept so far is still the best
    /// account of the child's death.
    pub fn run(mut self) {
        let mut ring = TailRing::default();
        let mut chunk = [0_u8; STDERR_READ_CHUNK_BYTES];
        loop {
            match self.pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    // A failed write to the hub's stderr must not stop the
                    // drain, or the child would block on a full pipe.
                    let _ = io::stderr().lock().write_all(&chunk[..read]);
                    ring.push(&chunk[..read]);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }

        let _ = self.sender.try_send(render_tail(ring.bytes.make_contiguous(), ring.wrapped, &self.redactions));
    }
}

/// Take `child`'s piped stderr and split its capture into the reader the
/// caller runs and the tail the caller keeps.
///
/// Fails only when the child was forked without a piped stderr.
pub fn tee_child_stderr(child: &mut Child, redactions: StderrRedactions) -> io::Result<(StderrTee, StderrTail)> {
    let pipe = child.stderr.take().ok_or_else(|| io::Error::other("the child's stderr was not piped"))?;
    let (sender, receiver) = sync_channel(1);
    Ok((StderrTee { pipe, redactions, sender }, StderrTail { receiver }))
}

/// The last [`STDERR_RING_BYTES`] of a child's stderr, and whether any
/// earlier bytes were dropped to keep it there.
#[derive(Default)]
struct TailRing {
    bytes: VecDeque<u8>,
    wrapped: bool,
}

impl TailRing {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let excess = self.bytes.len().saturating_sub(STDERR_RING_BYTES);
        if excess > 0 {
            self.bytes.drain(..excess);
            self.wrapped = true;
        }
    }
}

/// Render a stderr ring into the tail a spawn-failure detail carries.
///
/// In order: decode lossily; when the ring wrapped, drop the leading
/// partial line, so a path cut in half at the ring edge cannot survive
/// the redaction; strip terminal escapes and control characters; redact
/// the fork's host paths; trim; keep at most the last
/// [`STDERR_DETAIL_MAX_BYTES`].
fn render_tail(ring: &[u8], wrapped: bool, redactions: &StderrRedactions) -> String {
    let decoded = String::from_utf8_lossy(ring);
    let whole_lines = if wrapped {
        decoded.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &decoded
    };
    keep_last(redactions.apply(&strip_terminal_controls(whole_lines)).trim(), STDERR_DETAIL_MAX_BYTES)
}

/// Remove ANSI CSI sequences (ESC `[`, parameters, then a final byte in
/// `0x40..=0x7E`) and every control character other than newline and
/// tab. The child's tracing output is colored, and a detail is read as
/// plain text.
fn strip_terminal_controls(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            let mut rest = chars.clone();
            if rest.next() == Some('[') {
                chars = rest;
                chars.by_ref().find(|c| ('\u{40}'..='\u{7e}').contains(c));
            }
        } else if !c.is_control() || c == '\n' || c == '\t' {
            stripped.push(c);
        }
    }
    stripped
}

/// At most the last `max_bytes` of `text`, cut forward to a character
/// boundary and marked with a leading [`TRUNCATION_MARKER`] when cut.
fn keep_last(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let start = (text.len() - max_bytes..text.len()).find(|&index| text.is_char_boundary(index)).unwrap_or(text.len());
    format!("{TRUNCATION_MARKER}{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redactions for paths that exist nowhere, so only their as-given
    /// forms are needles and the test controls every byte.
    fn fictional_redactions(exec_path: &str, root: &str) -> StderrRedactions {
        StderrRedactions::for_fork(Path::new(exec_path), Path::new(root))
    }

    /// A tail over the cap is cut at a character boundary, not at a raw
    /// byte offset. Slicing a `str` mid-character panics, and inside the
    /// reader thread that panic would lose the tail entirely. The cut here
    /// lands on the second byte of `é`.
    #[test]
    fn an_over_cap_tail_cuts_at_a_character_boundary() {
        let text = format!("{}é{}", "x".repeat(STDERR_DETAIL_MAX_BYTES), "y".repeat(STDERR_DETAIL_MAX_BYTES - 1));

        let tail = render_tail(text.as_bytes(), false, &fictional_redactions("/nowhere/exec", "/nowhere"));

        assert!(
            tail.len() <= STDERR_DETAIL_MAX_BYTES + TRUNCATION_MARKER.len_utf8(),
            "tail exceeds the cap: {}",
            tail.len()
        );
        assert!(tail.starts_with(TRUNCATION_MARKER), "a cut tail is marked: {tail:?}");
        assert!(tail.ends_with('y'), "the cut keeps the end of the stream: {tail:?}");
    }

    /// A wrapped ring's cut-off first line is dropped, colors are
    /// stripped, and the executable path is redacted whole before the
    /// root it sits under. Replacing the root first would leave
    /// `<fleet-store>/<id>/<app name>` and leak the application-name file
    /// name; a surviving first line would leak a path fragment the
    /// redaction cannot recognise.
    #[test]
    fn a_wrapped_colored_tail_is_path_free() {
        let root = "/scratch/SENTINEL_ROOT/engines";
        let exec_path = format!("{root}/00000000000000000000000000000001/SentinelAppName");
        let ring = format!(
            "01/SentinelAppName: cut-off fragment\n\u{1b}[31merror\u{1b}[0m: running as {exec_path} under {root}\n"
        );

        let tail = render_tail(ring.as_bytes(), true, &fictional_redactions(&exec_path, root));

        assert!(tail.contains(SUBSTRATE_PLACEHOLDER), "the executable path is redacted: {tail:?}");
        assert!(!tail.contains("SENTINEL_ROOT"), "the fleet root must not survive: {tail:?}");
        assert!(!tail.contains("SentinelAppName"), "the application-name file name must not survive: {tail:?}");
        assert!(!tail.contains('\u{1b}'), "terminal escapes are stripped: {tail:?}");
    }
}
