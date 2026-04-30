// Substrate-side hub client. Dials an `aether-hub`, performs the
// Hello/Welcome handshake, then runs three background threads:
//
//   - a reader that blocks on `HubToEngine` frames and funnels inbound
//     `Mail` into the scheduler's `Mailer` after resolving the
//     recipient and kind against the local `Registry`,
//   - a writer that drains a `std::sync::mpsc::Receiver<EngineToHub>`
//     and serialises its frames onto the TCP stream,
//   - a heartbeat thread that pushes `EngineToHub::Heartbeat` onto the
//     writer's channel on a fixed cadence.
//
// Two threads writing to the same socket would race, so heartbeats and
// outbound mail frames both go through the single writer. The writer
// exits when the channel closes (when the HubClient drops) or when the
// socket errors; that in turn lets the OS close the socket and drop
// the reader/heartbeat threads.
//
// Per ADR-0006's "substrate stays sync" note, this module avoids
// `tokio` and uses the sync framing helpers from `aether-hub-protocol`.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::process;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_hub_protocol::{
    ClaudeAddress, EngineId, EngineMailFrame, EngineToHub, Hello, HubToEngine, KindDescriptor,
    MailByIdFrame, MailFrame, MailToEngineMailboxFrame, read_frame, write_frame,
};

use crate::mail::{Mail, ReplyTarget, ReplyTo};
use crate::mailer::Mailer;
use crate::registry::Registry;

/// Cadence at which this client emits `Heartbeat` to the hub. Must be
/// comfortably below the hub's read timeout (15s) so a single missed
/// tick doesn't trip reaping.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Shared outbound handle for the substrate side of the hub wire.
/// Registered once at `HubClient::connect` time. Sinks and other
/// components of the substrate hold an `Arc<HubOutbound>` and call
/// `send` without caring whether a hub connection is live — if it
/// isn't, the send silently drops. This lets the broadcast sink be
/// registered unconditionally, before (or without) a hub connection.
pub struct HubOutbound {
    tx: OnceLock<mpsc::Sender<EngineToHub>>,
}

impl HubOutbound {
    /// Fresh unwired handle. No frames are actually sent until a
    /// `HubClient::connect` attaches its writer channel via `attach`.
    pub fn disconnected() -> Arc<Self> {
        Arc::new(Self {
            tx: OnceLock::new(),
        })
    }

    /// Wire an outbound writer channel to this handle. Called once,
    /// either by `HubClient::connect` after the TCP handshake
    /// completes, or by a loopback chassis (ADR-0034 Phase 2) that
    /// drains `EngineToHub` frames in-process rather than serialising
    /// them over TCP. A second attach is ignored with a warning —
    /// `HubOutbound` is single-writer by design so heartbeats and
    /// outbound mail can't race on the socket (or the loopback
    /// channel).
    pub fn attach(&self, tx: mpsc::Sender<EngineToHub>) {
        if self.tx.set(tx).is_err() {
            tracing::warn!(target: "aether_substrate::hub_client", "HubOutbound attached twice — ignoring second attach");
        }
    }

    /// Push a frame onto the writer's channel. Returns `true` if the
    /// frame was enqueued; `false` if no hub is attached or the writer
    /// has exited.
    pub fn send(&self, frame: EngineToHub) -> bool {
        match self.tx.get() {
            Some(tx) => tx.send(frame).is_ok(),
            None => false,
        }
    }

    /// Encode `result` with postcard and send it as a reply addressed
    /// at `sender`. Forks on the sender variant: `Session` routes
    /// back to the Claude MCP session (ADR-0008); `EngineMailbox`
    /// routes via `EngineToHub::MailToEngineMailbox` to the hub,
    /// which forwards to the target engine's mailbox (ADR-0037
    /// Phase 2); `None` is a no-op (mail with no reply target).
    /// Silent on disconnected outbound and on encode failure.
    /// Returns `true` when the frame was enqueued on the writer
    /// channel.
    pub fn send_reply<K>(&self, sender: ReplyTo, result: &K) -> bool
    where
        K: aether_mail::Kind + serde::Serialize,
    {
        let payload = match postcard::to_allocvec(result) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    target: "aether_substrate::hub_client",
                    kind = K::NAME,
                    error = %e,
                    "reply encode failed",
                );
                return false;
            }
        };
        match sender.target {
            ReplyTarget::Session(token) => self.send(EngineToHub::Mail(EngineMailFrame {
                address: ClaudeAddress::Session(token),
                kind_name: K::NAME.to_owned(),
                payload,
                origin: None,
                correlation_id: sender.correlation_id,
            })),
            ReplyTarget::EngineMailbox {
                engine_id,
                mailbox_id,
            } => self.send(EngineToHub::MailToEngineMailbox(MailToEngineMailboxFrame {
                target_engine_id: engine_id,
                target_mailbox_id: mailbox_id,
                // Issue 469: wire frame fields are typed end-to-end.
                kind_id: K::ID,
                payload,
                count: 1,
                correlation_id: sender.correlation_id,
            })),
            // `Component` replies route through `Mailer::send_reply`,
            // not the hub — silently drop if a caller misroutes one
            // here rather than introduce a second truth for where
            // local replies go.
            ReplyTarget::None | ReplyTarget::Component(_) => false,
        }
    }

    /// Whether this handle has a live outbound channel.
    pub fn is_connected(&self) -> bool {
        self.tx.get().is_some()
    }

    /// Build an attached outbound paired with the receiver end so
    /// in-process drivers (tests, the test-bench's `TestBench` API
    /// per ADR-0067) can intercept frames the substrate would
    /// otherwise serialise to a hub. Mirrors the channel
    /// `HubClient::connect` attaches, minus the TCP machinery.
    pub fn attached_loopback() -> (Arc<Self>, mpsc::Receiver<EngineToHub>) {
        let (tx, rx) = mpsc::channel::<EngineToHub>();
        let outbound = Arc::new(Self {
            tx: OnceLock::new(),
        });
        outbound.attach(tx);
        (outbound, rx)
    }
}

/// Live hub connection. Threads are retained so their join handles
/// aren't dropped; they exit when the TCP socket closes or the
/// outbound channel is torn down.
pub struct HubClient {
    pub engine_id: EngineId,
    _reader: JoinHandle<()>,
    _writer: JoinHandle<()>,
    _heartbeat: JoinHandle<()>,
}

impl HubClient {
    /// Dial `addr`, send `Hello`, receive `Welcome`, and spawn the
    /// reader / writer / heartbeat threads. Inbound `Mail` is resolved
    /// and pushed onto `queue`; unknown recipient or kind names are
    /// logged and dropped.
    ///
    /// `outbound` is populated on success so any sink registered
    /// against it starts forwarding immediately.
    pub fn connect<A: ToSocketAddrs>(
        addr: A,
        name: impl Into<String>,
        version: impl Into<String>,
        kinds: Vec<KindDescriptor>,
        registry: Arc<Registry>,
        queue: Arc<Mailer>,
        outbound: Arc<HubOutbound>,
    ) -> io::Result<Self> {
        let mut stream = TcpStream::connect(addr)?;
        let hello = EngineToHub::Hello(Hello {
            name: name.into(),
            pid: process::id(),
            started_unix: unix_now(),
            version: version.into(),
            kinds,
        });
        write_frame(&mut stream, &hello).map_err(io::Error::other)?;

        let welcome: HubToEngine = read_frame(&mut stream).map_err(io::Error::other)?;
        let engine_id = match welcome {
            HubToEngine::Welcome(w) => w.engine_id,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected Welcome, got {other:?}"),
                ));
            }
        };
        tracing::info!(target: "aether_substrate::hub_client", engine_id = %engine_id.0, "hub registered engine");

        let (tx, rx) = mpsc::channel::<EngineToHub>();
        let reader_stream = stream.try_clone()?;
        let writer_stream = stream;
        let _reader = thread::spawn(move || run_reader(reader_stream, registry, queue));
        let _writer = thread::spawn(move || run_writer(writer_stream, rx));
        let heartbeat_tx = tx.clone();
        let _heartbeat = thread::spawn(move || run_heartbeat(heartbeat_tx));
        outbound.attach(tx);

        Ok(Self {
            engine_id,
            _reader,
            _writer,
            _heartbeat,
        })
    }
}

fn run_reader(mut stream: TcpStream, registry: Arc<Registry>, queue: Arc<Mailer>) {
    loop {
        match read_frame::<_, HubToEngine>(&mut stream) {
            Ok(HubToEngine::Mail(frame)) => dispatch_hub_to_engine_mail(frame, &registry, &queue),
            Ok(HubToEngine::MailById(frame)) => dispatch_hub_mail_by_id(frame, &registry, &queue),
            Ok(HubToEngine::Heartbeat) => {}
            Ok(HubToEngine::Welcome(_)) => {
                tracing::warn!(target: "aether_substrate::hub_client", "unexpected post-handshake Welcome, ignoring");
            }
            Ok(HubToEngine::Goodbye(g)) => {
                tracing::info!(target: "aether_substrate::hub_client", reason = %g.reason, "hub Goodbye");
                return;
            }
            Err(e) => {
                tracing::error!(target: "aether_substrate::hub_client", error = %e, "hub read error");
                return;
            }
        }
    }
}

fn run_writer(mut stream: TcpStream, rx: mpsc::Receiver<EngineToHub>) {
    while let Ok(frame) = rx.recv() {
        if let Err(e) = write_frame(&mut stream, &frame) {
            tracing::error!(target: "aether_substrate::hub_client", error = %e, "hub write error");
            return;
        }
    }
}

fn run_heartbeat(tx: mpsc::Sender<EngineToHub>) {
    loop {
        thread::sleep(HEARTBEAT_INTERVAL);
        if tx.send(EngineToHub::Heartbeat).is_err() {
            return;
        }
    }
}

/// Resolve a `HubToEngine::Mail` frame against the substrate's
/// `Registry` and push the decoded `Mail` onto `queue`. Shared by the
/// TCP `HubClient` reader (the canonical path) and the hub-chassis
/// loopback drainer (ADR-0034 Phase 2), so both paths drop on unknown
/// mailbox/kind with the same warning shape.
pub fn dispatch_hub_to_engine_mail(frame: MailFrame, registry: &Registry, queue: &Mailer) {
    let Some(recipient) = registry.lookup(&frame.recipient_name) else {
        tracing::warn!(
            target: "aether_substrate::hub_client",
            mailbox = %frame.recipient_name,
            "dropping hub mail to unknown mailbox",
        );
        return;
    };
    let Some(kind) = registry.kind_id(&frame.kind_name) else {
        tracing::warn!(
            target: "aether_substrate::hub_client",
            kind = %frame.kind_name,
            "dropping hub mail of unknown kind",
        );
        return;
    };
    queue.push(
        Mail::new(recipient, kind, frame.payload, frame.count)
            .with_reply_to(ReplyTo::to(ReplyTarget::Session(frame.sender))),
    );
}

/// Resolve a `HubToEngine::MailById` frame against the substrate's
/// `Registry` and push onto `queue`. Used by ADR-0037 Phase 2's
/// reply path — the hub forwards engine-mailbox replies as
/// id-addressed frames because the receiver's own mailbox name
/// didn't make it across the hash boundary when the sender bubbled
/// up. Public so the hub-chassis's engine read loop can call the
/// same helper from its own side of the wire.
pub fn dispatch_hub_mail_by_id(frame: MailByIdFrame, registry: &Registry, queue: &Mailer) {
    let kind = frame.kind_id;
    if registry.kind_name(kind).is_none() {
        tracing::warn!(
            target: "aether_substrate::hub_client",
            kind_id = %frame.kind_id,
            mailbox_id = %frame.recipient_mailbox_id,
            "MailById with unknown kind — dropped",
        );
        return;
    }
    queue.push(Mail::new(
        frame.recipient_mailbox_id,
        kind,
        frame.payload,
        frame.count,
    ));
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
