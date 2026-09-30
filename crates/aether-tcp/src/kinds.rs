//! Mail kinds owned by the `aether.tcp` capability family.
//!
//! The original 13 kind types plus the [`ListenerInfo`] helper struct
//! were formerly defined in `aether-kinds`; they live here now per
//! ADR-0121 (capabilities own their kinds). This module now owns 17
//! kinds. Kind ids are `fnv1a_64(name, schema)`, so moving declarations
//! does not change any id or alter wire compatibility.

use aether_actor::{HeldReply, PathRefused, ProtocolPath};
use serde::{Deserialize, Serialize};

/// What a tcp session delivers to its consumer: every reassembled frame and
/// the close notice, both silent (ADR-0231 §2).
///
/// A session holds its consumer as a `ProtocolRef<TcpConsumer>`, so its
/// fan-out compiles only for these two kinds. The explicit `consumer` field of
/// [`Connect`] and [`BindListener`] is a `ProtocolPath<TcpConsumer>`, and the
/// reflexive [`ConnectSelf`] and [`BindListenerSelf`] cast their sender to it,
/// so a consumer that would warn-drop either kind is refused before any
/// socket is dialed or bound.
#[aether_actor::protocol]
pub trait TcpConsumer {
    /// One reassembled length-prefix frame.
    fn data(mail: SessionData);
    /// The session closed: peer EOF, read error, or frame rejection.
    fn closed(mail: SessionClosed);
}

/// `aether.tcp.bind_listener` — request the singleton
/// `TcpCapability` to spawn a fresh `TcpListenerActor` bound to
/// `addr`. The cap parses `addr` via `std::net::ToSocketAddrs`
/// (so `"127.0.0.1:8080"` and `"0.0.0.0:0"` both work; the
/// latter asks the OS to pick a free port). Optional `name`
/// overrides the default subname (the bound port string); pass
/// `None` for the default. Reply: `BindListenerResult`.
///
/// Optional `consumer` is the canonical path of the actor every accepted
/// session delivers inbound frames and close notices to, an actor covering
/// [`TcpConsumer`] (ADR-0231 §3): in code an `ActorPath<R>` narrowed with
/// `.narrow::<TcpConsumer>()`, which compiles only when `R` handles both
/// kinds silently; over MCP the `path` a component load returns. A short
/// `root/:key` path is refused at decode with a warn and gets no reply; a path
/// no route has stood at, one whose route does not publish both silent rows,
/// and a consumer that has closed are answered
/// `Err(BindListenerError::Consumer(..))` without binding. `None` leaves the listener observer-less and drops
/// inbound bytes. A consumer binding itself sends [`BindListenerSelf`].
#[aether_data::kind(name = "aether.tcp.bind_listener", no_serde)]
pub struct BindListener {
    pub addr: String,
    pub name: Option<String>,
    pub consumer: Option<ProtocolPath<TcpConsumer>>,
}

/// `aether.tcp.bind_listener_self` — [`BindListener`] with the sender as
/// the consumer: every accepted session delivers its inbound frames and
/// close notices to the actor that sent this mail. The host-stamped sender
/// is already proven, so a component binds itself without naming its own
/// position; the cap casts it to [`TcpConsumer`] at receipt and replies
/// `Err` without binding when its published rows do not cover the protocol.
/// `addr` and `name` mean what they mean on [`BindListener`].
/// Reply: `BindListenerResult`.
#[aether_data::kind(name = "aether.tcp.bind_listener_self")]
pub struct BindListenerSelf {
    pub addr: String,
    pub name: Option<String>,
}

/// `aether.tcp.connect` — request the singleton `TcpCapability`
/// to dial `addr` and spawn a fresh `TcpSessionActor` over the
/// connected stream. Mirrors [`BindListener`]: `addr` is resolved
/// via `std::net::ToSocketAddrs`, and optional `name` overrides
/// the default `conn-N` session subname. Reply: [`ConnectResult`].
///
/// Optional `consumer` is the canonical path of the actor covering
/// [`TcpConsumer`] the dialed session delivers inbound frames and close
/// notices to, with [`BindListener`]'s rules: a short path is refused at
/// decode without a reply, and a never-registered, non-covering, or closed
/// consumer is answered `Err(ConnectError::Consumer(..))` without dialing.
/// `None` leaves the session observer-less and drops inbound bytes. A consumer dialing for itself sends [`ConnectSelf`].
#[aether_data::kind(name = "aether.tcp.connect", no_serde)]
pub struct Connect {
    pub addr: String,
    pub name: Option<String>,
    pub consumer: Option<ProtocolPath<TcpConsumer>>,
}

/// `aether.tcp.connect_self` — [`Connect`] with the sender as the
/// consumer: the dialed session delivers its inbound frames and close
/// notices to the actor that sent this mail. The host-stamped sender is
/// already proven, so a component dials without naming its own position;
/// the cap casts it to [`TcpConsumer`] at receipt and replies `Err` without
/// dialing when its published rows do not cover the protocol. `addr` and
/// `name` mean what they mean on [`Connect`]. Reply: [`ConnectResult`].
#[aether_data::kind(name = "aether.tcp.connect_self")]
pub struct ConnectSelf {
    pub addr: String,
    pub name: Option<String>,
}

/// Reply to [`Connect`] and [`ConnectSelf`]. `Ok` carries the resolved connect-session
/// subname and the connected peer address. `Err` names why no session was
/// dialed ([`ConnectError`]).
///
/// A consumer actor writes to the session through the host-stamped sender
/// of the [`SessionData`] it receives (`ctx.sender()`, then `ctx.send_to`).
/// An MCP agent addresses the session by the full ADR-0099 lineage path
/// `aether.tcp/aether.tcp.session:<session_name>` as a mail recipient address —
/// the bare subname is not a mailbox address.
#[aether_data::kind(name = "aether.tcp.connect_result")]
pub enum ConnectResult {
    Ok { session_name: String, peer: String },
    Err(ConnectError),
}

/// Why a [`Connect`] or [`ConnectSelf`] dialed no session.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The explicit `consumer` path did not prove (ADR-0231 §3): no route
    /// has stood at it, its route does not cover [`TcpConsumer`], or its
    /// actor has closed.
    Consumer(PathRefused),
    /// The requested address and a human-readable dial, spawn, or `_self`
    /// consumer failure.
    Failed { addr: String, error: String },
}

impl ConnectResult {
    /// The dial of `addr` failed for `error`.
    #[must_use]
    pub fn failed(addr: impl Into<String>, error: impl Into<String>) -> Self {
        Self::Err(ConnectError::Failed { addr: addr.into(), error: error.into() })
    }
}

impl From<PathRefused> for ConnectResult {
    fn from(refused: PathRefused) -> Self {
        Self::Err(ConnectError::Consumer(refused))
    }
}

impl HeldReply for ConnectResult {
    fn unanswered() -> Self {
        Self::failed(String::new(), "tcp capability closed before the connect completed")
    }
}

/// Reply to `BindListener`. `Ok` carries the resolved listener
/// name (the deterministic subname under
/// `aether.tcp.listener:<name>`) and the actually-bound local port
/// (load-bearing when `addr` requested port 0). `Err` names why nothing
/// was bound ([`BindListenerError`]).
///
/// The listener is addressed as
/// `aether.tcp/aether.tcp.listener:<listener_name>`.
#[aether_data::kind(name = "aether.tcp.bind_listener_result")]
pub enum BindListenerResult {
    Ok { listener_name: String, local_port: u16 },
    Err(BindListenerError),
}

/// Why a [`BindListener`] or [`BindListenerSelf`] bound nothing.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum BindListenerError {
    /// The explicit `consumer` path did not prove (ADR-0231 §3): no route
    /// has stood at it, its route does not cover [`TcpConsumer`], or its
    /// actor has closed.
    Consumer(PathRefused),
    /// The requested address and a human-readable reason: a `_self` consumer
    /// refusal, an addr parse failure, port-in-use, an OS bind error, or a
    /// namespace collision.
    Failed { addr: String, error: String },
}

impl BindListenerResult {
    /// The bind of `addr` failed for `error`.
    #[must_use]
    pub fn failed(addr: impl Into<String>, error: impl Into<String>) -> Self {
        Self::Err(BindListenerError::Failed { addr: addr.into(), error: error.into() })
    }
}

impl From<PathRefused> for BindListenerResult {
    fn from(refused: PathRefused) -> Self {
        Self::Err(BindListenerError::Consumer(refused))
    }
}

impl HeldReply for BindListenerResult {
    fn unanswered() -> Self {
        Self::failed(String::new(), "tcp capability closed before the bind completed")
    }
}

/// `aether.tcp.unbind_listener` — request the singleton
/// `TcpCapability` to close a listener by subname. The cap
/// looks the listener up in its own child map, mails
/// `Close` to it, monitors its close, and replies once
/// `MonitorNotice` arrives. Asynchronous reply: the response
/// only fires after the listener's accept thread has joined
/// and its slot has tombstoned.
#[aether_data::kind(name = "aether.tcp.unbind_listener")]
pub struct UnbindListener {
    pub listener_name: String,
}

/// Reply to `UnbindListener`. `Ok` once the listener has
/// tombstoned (the cap waited on `MonitorNotice` before
/// replying). `Err` for unknown listener names, listeners
/// already tombstoned at the time of the unbind request,
/// duplicate requests while an unbind is in progress, or fan-out
/// failures.
#[aether_data::kind(name = "aether.tcp.unbind_listener_result")]
pub enum UnbindListenerResult {
    Ok { listener_name: String },
    Err { listener_name: String, error: String },
}

impl HeldReply for UnbindListenerResult {
    fn unanswered() -> Self {
        Self::Err { listener_name: String::new(), error: "tcp capability closed before the unbind completed".into() }
    }
}

/// `aether.tcp.list_listeners` — enumerate every live listener
/// the singleton knows about. The cap walks its own child map
/// of live listeners. Reply: `ListListenersResult`.
#[aether_data::kind(name = "aether.tcp.list_listeners", default)]
pub struct ListListeners {}

/// One entry in `ListListenersResult`. `name` is the subname
/// (e.g. `"8080"`); `addr` is the requested bind addr passed
/// to `BindListener`; `port` is the actually-bound local port.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct ListenerInfo {
    pub name: String,
    pub addr: String,
    pub port: u16,
}

/// Reply to `ListListeners`. No `Err` arm: this is an enumeration of
/// the live fleet, and an enumeration cannot miss — no listeners is an
/// empty list, not a failure.
#[aether_data::kind(name = "aether.tcp.list_listeners_result", default)]
pub struct ListListenersResult {
    pub listeners: Vec<ListenerInfo>,
}

/// `aether.tcp.close` — peer asks a `TcpListenerActor` to
/// gracefully close. Mailed by `TcpCapability::on_unbind`; the
/// listener's handler signals its accept thread, joins, and
/// calls `ctx.shutdown()`. Fire-and-forget at the kind level
/// (the close response rides via the cap's monitor on the
/// listener, not via this kind).
#[aether_data::kind(name = "aether.tcp.close", default)]
pub struct Close {}

/// `aether.tcp.connection_ready` — sidecar accept thread → listener
/// dispatcher wake. Issue 607 Phase 6b: the listener's accept
/// thread blocks on `accept()`, pushes the resulting `TcpStream`
/// over an mpsc into the dispatcher, then fires this mail at its
/// own listener mailbox to wake the handler. The handler drains
/// the mpsc and spawns a `TcpSessionActor` per pending stream.
/// Empty payload — the actual stream rides the mpsc, not the mail
/// envelope (a live `TcpStream` is not wire-shaped).
#[aether_data::kind(name = "aether.tcp.connection_ready", default)]
pub struct ConnectionReady {}

/// `aether.tcp.connect_ready` — sidecar dial thread → capability
/// dispatcher wake. Mirror of [`ConnectionReady`] for outbound
/// connections: the dial thread pushes its `TcpStream` or error over
/// an mpsc and fires this fieldless mail so the cap can drain the
/// channel, spawn the session actor, and complete the parked reply.
#[aether_data::kind(name = "aether.tcp.connect_ready")]
pub struct ConnectReady {}

/// `aether.tcp.session_data_ready` — sidecar read thread → session
/// dispatcher wake. Mirror of [`ConnectionReady`] for the session
/// read path: the read thread blocks on `read()`, pushes bytes via
/// mpsc, fires this mail at its own session mailbox. The handler
/// drains the mpsc, reassembles length-prefixed frames, and delivers
/// each complete frame as [`SessionData`] to the session's bound
/// consumer. Empty payload.
#[aether_data::kind(name = "aether.tcp.session_data_ready", default)]
pub struct SessionDataReady {}

/// `aether.tcp.session_data` — one reassembled length-prefix frame
/// delivered by a `TcpSessionActor` to its configured consumer.
/// Carries the session subname (`conn-N`), the peer address as a
/// string, and the complete frame body. Structured-shaped
/// (variable-length payload) — agents drain via `receive_mail`.
#[aether_data::kind(name = "aether.tcp.session_data")]
pub struct SessionData {
    pub session_name: String,
    pub peer: String,
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
}

/// `aether.tcp.session_write` — peer mails this to a
/// `TcpSessionActor` to write `bytes` to the connected stream.
/// Fire-and-forget; the session's handler does a blocking write
/// on the dispatcher thread (writes are typically fast and
/// dispatcher-thread initiated, so a sidecar isn't needed for
/// the write path).
#[aether_data::kind(name = "aether.tcp.session_write")]
pub struct SessionWrite {
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
}

/// `aether.tcp.session_close` — peer asks the session to close
/// gracefully. A consumer actor closes the session through the
/// host-stamped sender of the [`SessionData`] it receives
/// (`ctx.sender()`, then `ctx.send_to`). The session's handler calls
/// `ctx.shutdown()`; the close fan-out fires `MonitorNotice` to
/// the parent actor that spawned it.
#[aether_data::kind(name = "aether.tcp.session_close", default)]
pub struct SessionClose {}

/// `aether.tcp.session_closed` — delivered to the session's configured
/// consumer on peer EOF, read error, or frame rejection. Carries the
/// session subname, the peer address, and a human-readable reason. A
/// trailing partial frame at close is dropped and noted in the reason.
#[aether_data::kind(name = "aether.tcp.session_closed")]
pub struct SessionClosed {
    pub session_name: String,
    pub peer: String,
    pub reason: String,
}
