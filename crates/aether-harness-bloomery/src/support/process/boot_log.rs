//! What a forked coordinator says about itself while it comes up.
//!
//! A child told to bind `:0` is the only process that knows which port it got,
//! and it already says so: the RPC ingress and the REST control API each log
//! `<name> server bound addr=… port=N` at `info` the moment their listener is
//! bound. Reading that line back is how the parent learns the port — no
//! reserve-then-release window for a sibling to steal, no platform socket table
//! to walk, and no chance of handshaking a stranger, because the port came from
//! the child's own mouth.
//!
//! The reader also keeps the last few stderr lines, so a child that dies before
//! it announces anything reports *why* instead of a bare deadline.
//!
//! A lane the coordinator dispatches inherits this same stderr, so the stream
//! is not the coordinator's alone. Each port is therefore latched at its first
//! announcement, which is the coordinator's own: both ingresses are bound
//! before the first order is ever dispatched, so nothing a lane says later can
//! move a port that a fixture is already dialling.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::ChildStderr;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

/// The line the RPC ingress logs once its listener is bound.
const RPC_BOUND: &str = "rpc server bound";

/// The same, for the REST control API.
const HTTP_BOUND: &str = "http server bound";

/// How many trailing stderr lines a boot log keeps for a failure message.
const TAIL_LINES: usize = 24;

/// Which ingress a waiter is asking about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ingress {
    /// The typed-mail `Call` ingress the wire fixtures dial.
    Rpc,
    /// The REST control API the operator drives with `curl`.
    Http,
}

impl Ingress {
    /// The announcement this ingress logs when it binds.
    const fn marker(self) -> &'static str {
        match self {
            Self::Rpc => RPC_BOUND,
            Self::Http => HTTP_BOUND,
        }
    }

    /// How a failure names this ingress.
    const fn label(self) -> &'static str {
        match self {
            Self::Rpc => "RPC",
            Self::Http => "HTTP",
        }
    }
}

/// A forked coordinator's stderr, read as it arrives.
///
/// Held by the [`Coordinator`](super::Coordinator) guard for as long as the
/// child is addressable. The reader thread ends at end-of-file, which is the
/// last holder of the write end closing it.
pub struct BootLog {
    announced: Arc<(Mutex<Announced>, Condvar)>,
}

/// What the reader thread has seen so far.
#[derive(Default)]
struct Announced {
    rpc_port: Option<u16>,
    http_port: Option<u16>,
    /// Stderr closed — the child is gone, so nothing further will be announced.
    closed: bool,
    tail: VecDeque<String>,
}

impl Announced {
    fn port(&mut self, ingress: Ingress) -> &mut Option<u16> {
        match ingress {
            Ingress::Rpc => &mut self.rpc_port,
            Ingress::Http => &mut self.http_port,
        }
    }

    fn record(&mut self, line: &str) {
        for ingress in [Ingress::Rpc, Ingress::Http] {
            if self.port(ingress).is_none()
                && let Some(port) = announced_port(line, ingress.marker())
            {
                *self.port(ingress) = Some(port);
            }
        }

        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(strip_ansi(line));
    }

    fn rendered_tail(&self) -> String {
        if self.tail.is_empty() {
            return "  (the child logged nothing)".to_owned();
        }
        self.tail.iter().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n")
    }
}

impl BootLog {
    /// Read `stderr` on a thread until the child closes it.
    #[must_use]
    pub fn draining(stderr: ChildStderr) -> Self {
        let announced = Arc::new((Mutex::new(Announced::default()), Condvar::new()));
        let reader = Arc::clone(&announced);
        #[allow(clippy::disallowed_methods)] // aether-suppression-request: harness-only stderr drain; ends at EOF
        thread::spawn(move || drain(stderr, &reader));

        Self { announced }
    }

    /// The port the child said it bound for `ingress`, waiting for the
    /// announcement until `deadline`.
    ///
    /// `Err` names the reason a waiter can act on: the child exited (with what
    /// it last logged), or it is still up and silent past the deadline.
    ///
    /// # Errors
    /// The child exited, or it did not announce the port in time.
    pub fn await_port(&self, ingress: Ingress, deadline: Instant) -> Result<u16, String> {
        let (announced, published) = &*self.announced;
        let mut announced = announced.lock().expect("the boot log lock is held only by non-panicking readers");
        loop {
            if let Some(port) = *announced.port(ingress) {
                return Ok(port);
            }
            if announced.closed {
                return Err(format!(
                    "the child exited before it announced its {} port; it last logged:\n{}",
                    ingress.label(),
                    announced.rendered_tail()
                ));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(format!(
                    "the child never announced its {} port; it last logged:\n{}",
                    ingress.label(),
                    announced.rendered_tail()
                ));
            };
            announced = published
                .wait_timeout(announced, remaining)
                .expect("the boot log lock is held only by non-panicking readers")
                .0;
        }
    }
}

fn drain(stderr: ChildStderr, announced: &Arc<(Mutex<Announced>, Condvar)>) {
    let (state, published) = &**announced;
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        let Ok(mut state) = state.lock() else {
            return;
        };
        state.record(&line);
        published.notify_all();
    }

    if let Ok(mut state) = state.lock() {
        state.closed = true;
        published.notify_all();
    }
}

/// The port `line` announces, when `line` is `marker`'s bind announcement.
///
/// The subscriber styles every field name, so `port` and its `=` arrive with
/// escape sequences wedged between them; matching against the raw line silently
/// finds nothing. Stripping first is what makes the match hold whether or not
/// the child's stderr is a terminal.
fn announced_port(line: &str, marker: &str) -> Option<u16> {
    let plain = strip_ansi(line);
    if !plain.contains(marker) {
        return None;
    }
    let digits = plain.rsplit_once("port=")?.1;
    digits.split(|character: char| !character.is_ascii_digit()).next()?.parse().ok()
}

/// `line` with its CSI escape sequences removed.
fn strip_ansi(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            plain.push(character);
            continue;
        }
        if characters.next() != Some('[') {
            continue;
        }
        for inside in characters.by_ref() {
            if matches!(inside, '@'..='~') {
                break;
            }
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::{HTTP_BOUND, RPC_BOUND, announced_port};

    /// The bind announcement exactly as the coordinator writes it: the
    /// subscriber styles the level, the target, and every field name, so
    /// `port=` never appears as four contiguous bytes.
    const STYLED_RPC: &str = "\u{1b}[2m2026-08-26T10:14:46.472784Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \
         \u{1b}[2maether_substrate::rpc\u{1b}[0m\u{1b}[2m:\u{1b}[0m rpc server bound \
         \u{1b}[3maddr\u{1b}[0m\u{1b}[2m=\u{1b}[0m127.0.0.1:0 \u{1b}[3mport\u{1b}[0m\u{1b}[2m=\u{1b}[0m54973";

    const STYLED_HTTP: &str = "\u{1b}[2m2026-08-26T10:14:46.472845Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \
         \u{1b}[2maether_http::server\u{1b}[0m\u{1b}[2m:\u{1b}[0m http server bound \
         \u{1b}[3maddr\u{1b}[0m\u{1b}[2m=\u{1b}[0m127.0.0.1:0 \u{1b}[3mport\u{1b}[0m\u{1b}[2m=\u{1b}[0m54974";

    // The plausible bug: the parser matches the raw line, the styling between
    // `port` and `=` hides the field, and every forked-child fixture burns its
    // budget waiting for a port the child announced on its first breath.
    #[test]
    fn a_styled_announcement_still_yields_its_port() {
        assert_eq!(announced_port(STYLED_RPC, RPC_BOUND), Some(54973));
        assert_eq!(announced_port(STYLED_HTTP, HTTP_BOUND), Some(54974));
    }

    // The plausible bug: the marker is ignored and any `port=` line answers, so
    // the RPC waiter takes the HTTP port and every handshake meets a server that
    // speaks HTTP back at it.
    #[test]
    fn one_ingress_does_not_answer_for_the_other() {
        assert_eq!(announced_port(STYLED_HTTP, RPC_BOUND), None);
        assert_eq!(announced_port(STYLED_RPC, HTTP_BOUND), None);
    }
}
