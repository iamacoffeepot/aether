//! aether-hub: substrate-side hub client + capability + the hub
//! coordinator itself (ADR-0070 phases 4-5 / ADR-0071 phase 7,
//! ADR-0072 fold of `aether-hub-protocol`).
//!
//! Houses everything that needs to know the hub wire format on the
//! substrate side:
//!
//! - [`HubProtocolBackend`] — `EgressBackend` impl that translates each
//!   substrate egress intent into the matching `EngineToHub` frame and
//!   pushes it onto a writer channel.
//! - [`HubClient`] — TCP dial + Hello/Welcome handshake + reader /
//!   writer / heartbeat thread cluster.
//! - [`HubClientCapability`] — passive `Capability` impl that wraps
//!   [`HubClient::connect`] for chassis composition via
//!   `Builder::with()`. Connects on boot when its URL is `Some`;
//!   no-ops when `None` so chassis can opt out cleanly.
//! - [`loopback_outbound`] — substitute for an in-process driver
//!   (test-bench, hub-chassis loopback) that wants to drain
//!   `EngineToHub` frames the substrate would otherwise serialise.
//! - [`dispatch_hub_to_engine_mail`] / [`dispatch_hub_mail_by_id`] —
//!   inbound-frame resolvers shared by [`HubClient`]'s reader and the
//!   hub-chassis's loopback drainer.
//!
//! Plus the hub coordinator itself (ADR-0071 phase 7d-2 relocated
//! these from the `aether-substrate-hub` binary crate):
//!
//! - [`HubChassis`] / [`HubServerCapability`] — Chassis marker +
//!   driver capability that owns the tokio runtime + listeners.
//! - [`run_engine_listener`] — the engine-facing TCP listener loop.
//! - [`run_mcp_server`] — the rmcp-driven MCP transport on
//!   [`DEFAULT_MCP_PORT`].
//! - [`EngineRegistry`] / [`SessionRegistry`] / [`PendingSpawns`] /
//!   [`LogStore`] — the in-process state the coordinator wires
//!   together.
//! - [`spawn_substrate`] / [`terminate_substrate`] — child-process
//!   lifecycle for the `spawn_substrate` MCP tool.
//!
//! Per ADR-0006's "substrate stays sync" note, the hub-client
//! machinery uses `std::sync::mpsc` and the sync framing helpers from
//! [`aether_codec::frame`]. The coordinator itself is async (tokio).

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_codec::frame::{read_frame, write_frame};
use aether_data::{KindDescriptor, KindId, MailboxId};
use aether_substrate_core::{
    BootError, Capability, ChassisCtx, EgressBackend, HubOutbound, LogEntry as SubstrateLogEntry,
    LogLevel as SubstrateLogLevel, Mail, Mailer, Registry, ReplyTarget, ReplyTo, RunningCapability,
    SubstrateBoot,
};
use tokio::net::TcpListener;

use crate::wire::{
    ClaudeAddress, EngineId, EngineMailFrame, EngineMailToHubSubstrateFrame, EngineToHub, Hello,
    HubToEngine, LogEntry as HubLogEntry, LogLevel as HubLogLevel, MailByIdFrame, MailFrame,
    MailToEngineMailboxFrame, SessionToken,
};

mod chassis;
mod engine;
mod log_store;
mod loopback;
mod mcp;
mod registry;
mod session;
mod spawn;
pub mod wire;

pub use aether_codec::{DecodeError, EncodeError, decode_schema, encode_schema};
pub use aether_substrate_core::Chassis;
pub use chassis::{HubChassis, HubEnv, HubServerCapability, HubServerRunning};
pub use engine::READ_TIMEOUT;
pub use log_store::{LogStore, ReadResult as LogReadResult};
pub use loopback::{HUB_SELF_ENGINE_ID, LoopbackEngine, LoopbackHandle};
pub use mcp::{DEFAULT_MCP_PORT, HubState, run_mcp_server};
pub use registry::{EngineRecord, EngineRegistry};
pub use session::{
    QueuedMail, SESSION_CHANNEL_CAPACITY, SessionHandle, SessionRecord, SessionRegistry,
};
pub use spawn::{
    DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_TERMINATE_GRACE, PendingSpawns, SpawnError, SpawnOpts,
    TerminateOutcome, spawn_substrate, terminate_substrate,
};

/// Default port the hub binds for engine TCP clients. ADR-0006 V0
/// fixes this; `AETHER_ENGINE_PORT` overrides.
pub const DEFAULT_ENGINE_PORT: u16 = 8889;

/// Run the engine listener loop on `addr`, dispatching each accepted
/// connection to a per-connection task. Returns on listener error
/// only; individual connection failures are logged and isolated.
pub async fn run_engine_listener(
    addr: SocketAddr,
    registry: EngineRegistry,
    sessions: SessionRegistry,
    pending: PendingSpawns,
    logs: LogStore,
    loopback: loopback::LoopbackHandle,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("aether-substrate-hub: engine listener bound on {bound}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        let sessions = sessions.clone();
        let pending = pending.clone();
        let logs = logs.clone();
        let loopback = loopback.clone();
        tokio::spawn(async move {
            if let Err(e) =
                engine::handle_connection(stream, registry, sessions, pending, logs, loopback).await
            {
                eprintln!("aether-substrate-hub: engine {peer} dropped: {e}");
            }
        });
    }
}

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

/// ADR-0071 phase 7 passive `Capability`: dials a hub at boot and
/// retains the [`HubClient`]'s reader / writer / heartbeat threads for
/// the chassis lifetime. `url == None` (or the empty string) is the
/// no-op case — the capability boots successfully without dialing,
/// retaining no threads. Boot failures (TCP dial, handshake, frame
/// errors) propagate as [`BootError::Other`] so the chassis fail-fast
/// path runs (ADR-0063).
///
/// The capability constructor takes the `Arc<HubOutbound>` from the
/// chassis's [`aether_substrate_core::SubstrateBoot`] explicitly:
/// substrate-core's `Builder` doesn't surface outbound through
/// `ChassisCtx`, and the wiring relationship between the boot's
/// outbound and a hub connection is naturally set at chassis-
/// composition time. A successful connect calls
/// `outbound.attach_backend(...)` which is what makes substrate-side
/// egress flow upward through the TCP socket.
pub struct HubClientCapability {
    url: Option<String>,
    name: String,
    version: String,
    kinds: Vec<KindDescriptor>,
    outbound: Arc<HubOutbound>,
}

impl HubClientCapability {
    /// Construct a fresh capability. `url` is the engine-side hub URL
    /// (typically `AETHER_HUB_URL`); `None` or empty boots cleanly
    /// without dialing. `name` and `version` ride on the `Hello`
    /// frame; `kinds` is the engine's full kind-descriptor list (the
    /// same list the boot emits — clone from
    /// `SubstrateBoot::boot_descriptors`). `outbound` is the boot's
    /// `Arc<HubOutbound>`; the capability calls
    /// [`HubOutbound::attach_backend`] on it after a successful
    /// connect so substrate-side egress starts forwarding through the
    /// TCP socket.
    pub fn new(
        url: Option<String>,
        name: impl Into<String>,
        version: impl Into<String>,
        kinds: Vec<KindDescriptor>,
        outbound: Arc<HubOutbound>,
    ) -> Self {
        Self {
            url,
            name: name.into(),
            version: version.into(),
            kinds,
            outbound,
        }
    }
}

/// Post-boot handle for [`HubClientCapability`]. Holds the live
/// [`HubClient`] (its `JoinHandle`s) when the capability successfully
/// dialed; `None` when the constructor's `url` was `None` or empty.
/// On chassis shutdown the [`RunningCapability::shutdown`] impl drops
/// the client; the writer thread observes the dropped channel and
/// exits, the reader thread observes the closed socket and exits, and
/// the heartbeat thread observes the closed channel and exits.
pub struct HubClientRunning {
    pub client: Option<HubClient>,
}

impl Capability for HubClientCapability {
    type Running = HubClientRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let url = match self.url.as_deref() {
            Some(u) if !u.is_empty() => u.to_owned(),
            _ => return Ok(HubClientRunning { client: None }),
        };
        let registry = Arc::clone(ctx.registry());
        let queue = Arc::clone(ctx.mailer());
        let client = HubClient::connect(
            url.as_str(),
            self.name,
            self.version,
            self.kinds,
            registry,
            queue,
            self.outbound,
        )
        .map_err(|e| {
            BootError::Other(Box::new(io::Error::other(format!(
                "hub connect to {url:?} failed: {e}"
            ))))
        })?;
        Ok(HubClientRunning {
            client: Some(client),
        })
    }
}

impl RunningCapability for HubClientRunning {
    fn shutdown(self: Box<Self>) {
        // Dropping the `HubClient` drops its writer/reader/heartbeat
        // join handles, which detaches them — they exit naturally
        // when the TCP socket closes or the outbound channel drains.
        // No explicit join: chassis shutdown is best-effort about hub
        // teardown (ADR-0063 fail-fast already handles the abort
        // case via `flush_now`).
        drop(self);
    }
}

/// Drop-in replacement for the pre-refactor `boot.connect_hub(url)`.
/// Dials the hub if `url` is `Some(non-empty)` and returns the live
/// [`HubClient`] handle; returns `Ok(None)` for `None` / empty.
/// Chassis crates that prefer explicit handle ownership over the
/// [`HubClientCapability`] / `Builder::with()` adoption use this.
///
/// Equivalent to constructing a `HubClientCapability` and booting it
/// against the boot's registry / mailer / outbound, but synchronous —
/// returns the `HubClient` directly so the chassis can stash it next
/// to its own state.
pub fn connect_hub_client(
    boot: &SubstrateBoot,
    url: Option<&str>,
) -> wasmtime::Result<Option<HubClient>> {
    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => return Ok(None),
    };
    let client = HubClient::connect(
        url,
        &boot.name,
        &boot.version,
        boot.boot_descriptors.clone(),
        Arc::clone(&boot.registry),
        Arc::clone(&boot.queue),
        Arc::clone(&boot.outbound),
    )
    .map_err(|e| wasmtime::Error::msg(format!("hub connect to {url:?} failed: {e}")))?;
    Ok(Some(client))
}
