//! aether-hub: substrate-side hub client + the thin hub chassis
//! (ADR-0070 phases 4-5 / ADR-0071 phase 7, ADR-0072 fold of
//! `aether-hub-protocol`).
//!
//! Houses everything that needs to know the hub wire format on the
//! substrate side:
//!
//! - [`HubProtocolBackend`] — `EgressBackend` impl that translates each
//!   substrate egress intent into the matching `EngineToHub` frame and
//!   pushes it onto a writer channel.
//! - [`HubClient`] — TCP dial + Hello/Welcome handshake + reader /
//!   writer / heartbeat thread cluster.
//! - [`connect_hub_client`] — function-shaped wrapper around
//!   [`HubClient::connect`] for chassis composition. Connects when
//!   its URL is `Some`; no-ops when `None` so chassis can opt out
//!   cleanly.
//! - [`loopback_outbound`] — substitute for an in-process driver
//!   (test-bench) that wants to drain `EngineToHub` frames the
//!   substrate would otherwise serialise.
//! - [`dispatch_hub_to_engine_mail`] / [`dispatch_hub_mail_by_id`] —
//!   inbound-frame resolvers shared by [`HubClient`]'s reader.
//!
//! Plus the hub chassis itself (post-issue-763 P5f):
//!
//! - [`HubChassis`] / [`HubServerDriverCapability`] — Chassis marker +
//!   driver capability. The hub is now a thin coordinator: it stands
//!   up `TraceObserverCapability` + `EngineServer` +
//!   `RpcServerCapability` and blocks on SIGINT/SIGTERM. The OLD
//!   `EngineToHub` TCP listener, hub-side sessions,
//!   `ProcessCapability`, loopback drainers, and embedded MCP server
//!   all retired with P5e/P5f.
//!
//! Per ADR-0006's "substrate stays sync" note, the hub-client
//! machinery uses `std::sync::mpsc` and the sync framing helpers from
//! [`aether_codec::frame`].

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::process;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_codec::frame::{read_frame, write_frame};
use aether_data::{KindDescriptor, KindId, MailboxDescriptor, MailboxId};
use aether_substrate::{
    EgressBackend, HubOutbound, LogEntry as SubstrateLogEntry, LogLevel as SubstrateLogLevel, Mail,
    Mailer, Registry, ReplyTarget, ReplyTo, SubstrateBoot,
};

use crate::hub::wire::{
    ClaudeAddress, EngineId, EngineMailFrame, EngineMailToHubSubstrateFrame, EngineToHub, Hello,
    HubToEngine, LogEntry as HubLogEntry, LogLevel as HubLogLevel, MailByIdFrame, MailFrame,
    MailToEngineMailboxFrame, SessionToken,
};

mod chassis;
pub mod wire;

pub use aether_codec::{DecodeError, EncodeError, decode_schema, encode_schema};
pub use aether_substrate::Chassis;
pub use chassis::{HubChassis, HubEnv, HubServerDriverCapability, HubServerDriverRunning};

/// Default port the hub binds its `aether.rpc.server` on (issue 763).
/// The hub boots its RPC server unconditionally — it's the target the
/// out-of-process `aether-mcp` coordinator dials (matching that
/// crate's `DEFAULT_HUB_RPC_ADDR`). `AETHER_RPC_PORT` overrides.
pub const DEFAULT_RPC_PORT: u16 = 8901;

/// Cadence at which this client emits `Heartbeat` to the hub. Must be
/// comfortably below the hub's read timeout (15s) so a single missed
/// tick doesn't trip reaping.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// `EgressBackend` impl that translates each substrate-side egress
/// intent into the matching `EngineToHub` wire frame and pushes it
/// onto the writer channel. Wired by [`HubClient::connect`] after the
/// TCP handshake completes; substrate-core is unaware of the
/// translation.
///
/// `is_connected` reports `true` while the channel send-half is
/// reachable — a closed writer channel (writer thread exited on
/// socket error) flips it to `false` on the next failed send so
/// bubble-up gracefully degrades to local warn-drop without needing a
/// reconnect. The mpsc channel itself can't be polled for liveness;
/// senders learn the channel is closed only by attempting a `send`
/// that returns `Err`. We track that via an `AtomicBool` flipped on
/// the first failed send so subsequent calls short-circuit.
pub struct HubProtocolBackend {
    tx: mpsc::Sender<EngineToHub>,
    alive: std::sync::atomic::AtomicBool,
}

impl HubProtocolBackend {
    /// Wrap a writer-channel sender. Most callers reach this through
    /// [`HubClient::connect`]; tests use [`loopback_outbound`] for a
    /// sender + receiver pair.
    pub fn new(tx: mpsc::Sender<EngineToHub>) -> Self {
        Self {
            tx,
            alive: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn send(&self, frame: EngineToHub) {
        use std::sync::atomic::Ordering;
        if !self.alive.load(Ordering::Relaxed) {
            return;
        }
        if self.tx.send(frame).is_err() {
            self.alive.store(false, Ordering::Relaxed);
        }
    }

    fn level_to_wire(level: SubstrateLogLevel) -> HubLogLevel {
        match level {
            SubstrateLogLevel::Trace => HubLogLevel::Trace,
            SubstrateLogLevel::Debug => HubLogLevel::Debug,
            SubstrateLogLevel::Info => HubLogLevel::Info,
            SubstrateLogLevel::Warn => HubLogLevel::Warn,
            SubstrateLogLevel::Error => HubLogLevel::Error,
        }
    }

    fn entry_to_wire(entry: SubstrateLogEntry) -> HubLogEntry {
        HubLogEntry {
            timestamp_unix_ms: entry.timestamp_unix_ms,
            level: Self::level_to_wire(entry.level),
            target: entry.target,
            message: entry.message,
            sequence: entry.sequence,
            origin: entry.origin,
        }
    }
}

impl EgressBackend for HubProtocolBackend {
    fn is_connected(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn egress_to_session(
        &self,
        session: SessionToken,
        kind_name: &str,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    ) {
        self.send(EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Session(session),
            kind_name: kind_name.to_owned(),
            payload,
            origin,
            correlation_id,
        }));
    }

    fn egress_broadcast(
        &self,
        kind_name: &str,
        payload: Vec<u8>,
        origin: Option<String>,
        correlation_id: u64,
    ) {
        self.send(EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Broadcast,
            kind_name: kind_name.to_owned(),
            payload,
            origin,
            correlation_id,
        }));
    }

    fn egress_to_engine_mailbox(
        &self,
        engine_id: EngineId,
        mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        correlation_id: u64,
    ) {
        self.send(EngineToHub::MailToEngineMailbox(MailToEngineMailboxFrame {
            target_engine_id: engine_id,
            target_mailbox_id: mailbox_id,
            kind_id,
            payload,
            count,
            correlation_id,
        }));
    }

    fn egress_unresolved_mail(
        &self,
        recipient_mailbox_id: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
        count: u32,
        source_mailbox_id: Option<MailboxId>,
        correlation_id: u64,
    ) {
        self.send(EngineToHub::MailToHubSubstrate(
            EngineMailToHubSubstrateFrame {
                recipient_mailbox_id,
                kind_id,
                payload,
                count,
                source_mailbox_id,
                correlation_id,
            },
        ));
    }

    fn egress_kinds_changed(&self, descriptors: Vec<KindDescriptor>) {
        self.send(EngineToHub::KindsChanged(descriptors));
    }

    fn egress_mailboxes_changed(&self, descriptors: Vec<MailboxDescriptor>) {
        self.send(EngineToHub::MailboxesChanged(descriptors));
    }

    fn egress_log_batch(&self, entries: Vec<SubstrateLogEntry>) {
        let wire: Vec<HubLogEntry> = entries.into_iter().map(Self::entry_to_wire).collect();
        self.send(EngineToHub::LogBatch(wire));
    }
}

/// Build an outbound facade pre-attached to a fresh
/// [`HubProtocolBackend`], returning the receiver end so in-process
/// drivers (the test-bench, the hub-chassis loopback) can drain
/// `EngineToHub` frames the substrate would otherwise have written to
/// a TCP socket.
pub fn loopback_outbound() -> (Arc<HubOutbound>, mpsc::Receiver<EngineToHub>) {
    let (tx, rx) = mpsc::channel::<EngineToHub>();
    let outbound = HubOutbound::disconnected();
    outbound.attach_backend(Arc::new(HubProtocolBackend::new(tx)));
    (outbound, rx)
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
    // Wire-shaped fan-in (addr + name + version + kinds + mailboxes +
    // 3 substrate handles) — clippy reads as too-many but every arg is
    // a distinct shape with no obvious grouping; bundling would just
    // be artificial nesting.
    #[allow(clippy::too_many_arguments)]
    pub fn connect<A: ToSocketAddrs>(
        addr: A,
        name: impl Into<String>,
        version: impl Into<String>,
        kinds: Vec<KindDescriptor>,
        mailboxes: Vec<MailboxDescriptor>,
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
            mailboxes,
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
        tracing::info!(
            target: "aether_hub::client",
            engine_id = %engine_id.0,
            "hub registered engine",
        );

        let (tx, rx) = mpsc::channel::<EngineToHub>();
        let reader_stream = stream.try_clone()?;
        let writer_stream = stream;
        let _reader = thread::spawn(move || run_reader(reader_stream, registry, queue));
        let _writer = thread::spawn(move || run_writer(writer_stream, rx));
        let heartbeat_tx = tx.clone();
        let _heartbeat = thread::spawn(move || run_heartbeat(heartbeat_tx));
        outbound.attach_backend(Arc::new(HubProtocolBackend::new(tx)));

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
                tracing::warn!(
                    target: "aether_hub::client",
                    "unexpected post-handshake Welcome, ignoring",
                );
            }
            Ok(HubToEngine::Goodbye(g)) => {
                tracing::info!(target: "aether_hub::client", reason = %g.reason, "hub Goodbye");
                return;
            }
            Err(e) => {
                tracing::error!(target: "aether_hub::client", error = %e, "hub read error");
                return;
            }
        }
    }
}

fn run_writer(mut stream: TcpStream, rx: mpsc::Receiver<EngineToHub>) {
    while let Ok(frame) = rx.recv() {
        if let Err(e) = write_frame(&mut stream, &frame) {
            tracing::error!(target: "aether_hub::client", error = %e, "hub write error");
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
/// TCP [`HubClient`] reader (the canonical path) and the hub-chassis
/// loopback drainer (ADR-0034 Phase 2), so both paths drop on unknown
/// mailbox/kind with the same warning shape.
pub fn dispatch_hub_to_engine_mail(frame: MailFrame, registry: &Registry, queue: &Mailer) {
    let Some(recipient) = registry.lookup(&frame.recipient_name) else {
        tracing::warn!(
            target: "aether_hub::client",
            mailbox = %frame.recipient_name,
            "dropping hub mail to unknown mailbox",
        );
        return;
    };
    let Some(kind) = registry.kind_id(&frame.kind_name) else {
        tracing::warn!(
            target: "aether_hub::client",
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
            target: "aether_hub::client",
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

/// Synchronous hub-client connect. Dials the hub if `url` is
/// `Some(non-empty)` and returns the live [`HubClient`] handle;
/// returns `Ok(None)` for `None` / empty. A successful connect calls
/// `outbound.attach_backend(...)` which is what makes substrate-side
/// egress flow upward through the TCP socket. Pre-PR-E3 there was
/// also a `HubClientCapability` wrapper that fit the cap-builder
/// chain via `Capability::boot`; that wrapper retired with
/// `Capability` itself — every chassis now uses this function
/// directly.
pub fn connect_hub_client(
    boot: &SubstrateBoot,
    url: Option<&str>,
) -> anyhow::Result<Option<HubClient>> {
    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => return Ok(None),
    };
    let client = HubClient::connect(
        url,
        &boot.name,
        &boot.version,
        boot.boot_descriptors.clone(),
        boot.registry.list_mailbox_descriptors(),
        Arc::clone(&boot.registry),
        Arc::clone(&boot.queue),
        Arc::clone(&boot.outbound),
    )
    .map_err(|e| anyhow::anyhow!("hub connect to {url:?} failed: {e}"))?;

    // Issue iamacoffeepot/aether#742: install the registry's
    // mailbox-change hook so every subsequent registration (the
    // chassis-builder `.with_actor::<...>` chain that runs *after*
    // this connect, plus runtime `load_component` trampoline
    // registrations, plus any future registration path) republishes
    // the inventory to the hub. Without this the Hello frame above is
    // the only inventory the hub ever sees, missing every chassis
    // cap that registers post-connect — those would then render as
    // bare `mbx-XXXX-XXXX-XXXX` ids in the trace tools.
    let outbound_for_hook = Arc::clone(&boot.outbound);
    boot.registry
        .set_on_mailbox_change(Arc::new(move |descriptors| {
            outbound_for_hook.egress_mailboxes_changed(descriptors);
        }));

    Ok(Some(client))
}
