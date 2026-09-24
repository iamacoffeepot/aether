//! The `aether.rpc.server` runtime half (ADR-0122 identity/runtime split).
//! The [`RpcServerCapability`] identity file
//! names none of these types. The substrate-typed
//! imports are collected once by this module rather than line-by-line; the
//! `#[actor] impl` in the parent reaches the state, ctx types, the
//! `RpcServerHandle` boot artifact, and the per-connection helpers through
//! the single `use runtime::*` glob.
//!
//! The accept thread (spawned by [`RpcServerState::start_accepting`], from
//! `init` under [`RpcBind::Boot`] or when a held server's [`RpcBindGate`]
//! opens) and the per-connection reader threads (spawned in
//! [`RpcServerState::spawn_reader_for_peer`]) capture only cloned channels and
//! a [`SelfWake`] handle built in `init` or cloned out of the state — never the
//! `RpcServerState` value, and never a mailbox position (ADR-0230). The handle
//! wakes the cap and can send nothing else. The gate carries the same two
//! things: a sender on the inbound channel and a wake-only [`SelfWake`].

// `#[handler]` methods take their decoded payload by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the decoded bytes so
// callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Sibling / cap-level types named by the state, the helpers, and the
// `#[runtime] impl NativeActor` block below, reached through the parent
// module. `super::` works because `runtime` is a descendant of `server` (the
// parent's private `use` aliases + the `pub` connection items are
// visible to it). `RpcServerConfig` is named by `init`'s signature; the cap
// struct `RpcServerCapability` is the impl's `Self` type.
use super::connection::{ConnId, ConnState, InboundEvent, run_reader_loop};
use super::{
    MonitorNotice, PeerKind, RegisterEngineRoute, RpcBind, RpcInboundReady, RpcServerCapability, RpcServerConfig,
    RpcServerParams, Settled,
};
use aether_actor::{HandlesKind, runtime};
use aether_substrate::atomic_write::atomic_write;
use aether_substrate::mail::boundary::is_engine_only;
use aether_substrate::net::teardown_connect_addr;

// Re-export every substrate / std / cross-crate type the top-level
// `#[actor] impl` body in `mod.rs` names; it reaches them through the
// single `use runtime::*` glob. Types named only by the inherent helper
// methods below ride the same wall (used locally here).
pub use crate::kinds::{CallSettled, ForwardEnvelope, RegisterEngineRouteResult};
pub use crate::{Hello, HelloAck, MailEnvelope, ReplyEnvelope, RpcError, WIRE_VERSION, WireFrame};
pub use aether_actor::ErasedActorRef;
pub use aether_codec::frame::{FrameError, write_frame};
pub use aether_data::{EngineId, Kind};
pub use aether_substrate::MonitorHandle;
pub use aether_substrate::actor::native::envelope::Envelope;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SelfWake};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::mailer::Mailer;
pub use std::collections::{HashMap, HashSet};
pub use std::io;
pub use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::sync::mpsc;
pub use std::thread::JoinHandle;
pub use std::time::Duration;

/// Exported handle bundle published at boot. Reachable from the
/// chassis via `PassiveChassis::handle::<RpcServerHandle>()`;
/// the load-bearing field is `local_port` so embedders (driver
/// threads, tests) can connect to the OS-picked port when
/// `bind_addr` requested port 0.
#[derive(Clone)]
pub struct RpcServerHandle {
    pub local_port: u16,
}

/// The boot artifact a held server publishes in place of an
/// [`RpcServerHandle`] (issue #6399). A server composed with
/// [`RpcBind::Held`] and a resolved port binds nothing in `init`; its composer
/// reads this off the built chassis with `handle::<RpcBindGate>()` and calls
/// [`Self::open`] once everything a caller may address is live. Until then a
/// dial is refused, so a caller that reaches the server can address the whole
/// engine.
///
/// It holds the resolved port, a sender on the cap's inbound channel, a
/// wake-only [`SelfWake`] (ADR-0230: no mailbox position), and the flag that
/// makes a second open fail. A server composed without a port publishes no
/// gate.
#[derive(Clone)]
pub struct RpcBindGate {
    port: u16,
    /// Where [`Self::open`] reports the port it bound, when configured.
    port_file: Option<PathBuf>,
    inbound: mpsc::Sender<InboundEvent>,
    wake: SelfWake<RpcInboundReady>,
    opened: Arc<AtomicBool>,
}

impl RpcBindGate {
    /// Bind `127.0.0.1:{port}` on the calling thread, hand the listener to
    /// the cap, wake it, report the bound port to the configured port file,
    /// and return it (the OS-picked one when the resolved port is `0`).
    ///
    /// The listener is listening when this returns, and when the port file
    /// appears: a dial that lands before the cap's next turn starts the
    /// accept thread waits in the backlog rather than being refused.
    ///
    /// # Errors
    ///
    /// Returns an error when the gate was already opened (whether that open
    /// bound or not), when the port cannot be bound, when the server has
    /// already stopped and cannot take the listener, or when the port file
    /// cannot be written.
    pub fn open(&self) -> io::Result<u16> {
        if self.opened.swap(true, Ordering::AcqRel) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "rpc bind gate is already open"));
        }

        let listener = TcpListener::bind(("127.0.0.1", self.port))?;
        let port = listener.local_addr()?.port();
        self.inbound
            .send(InboundEvent::Bound { listener })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "rpc server stopped before its gate opened"))?;
        self.wake.wake(&RpcInboundReady::default());
        if let Some(path) = &self.port_file {
            write_port_file(path, port)?;
        }
        Ok(port)
    }
}

/// Report the port this server bound: its decimal form and a newline,
/// written atomically so a reader sees the whole port or no file.
fn write_port_file(path: &Path, port: u16) -> io::Result<()> {
    atomic_write(path, format!("{port}\n").as_bytes())
}

/// Bookkeeping for one in-flight call (cid passed `Some` on the
/// wire). Looked up by the dispatch's auto-minted
/// `correlation_id` (== `MailId.correlation_id` of the dispatched
/// envelope, which is also the root id since we always dispatch
/// as chassis-root via `send_envelope_detached_to`). Fields are
/// `pub` so the parent's `on_settled` / `on_any` handlers can
/// read them after `remove` / `get`.
#[derive(Copy, Clone)]
pub struct InFlight {
    pub conn_id: ConnId,
    pub wire_cid: u64,
    /// The registered proxy a forwarded engine call went to, so closing
    /// the call removes its correlation from that owner's
    /// [`RouteOwner::calls`] with one keyed lookup. `None` for a call
    /// dispatched into this server's local actor system.
    pub route: Option<ErasedActorRef>,
}

/// One registered engine route, keyed by its registrant in
/// [`RpcServerState::route_owners`]: what the registrant's
/// `MonitorNotice` must retire, reachable without a scan.
pub struct RouteOwner {
    /// The one engine this registrant forwards for; its key in
    /// [`RpcServerState::engine_routes`].
    pub engine: EngineId,
    /// Correlations of the forwarded calls still in flight at this
    /// registrant, so its departure closes exactly those calls.
    pub calls: HashSet<u64>,
    /// Keeps the registrant monitored; `Drop` deregisters. `None` for a
    /// registrant that could not be monitored, whose route then lasts
    /// until this server stops.
    _monitor: Option<MonitorHandle>,
}

/// `aether.rpc.server` runtime state (ADR-0122 split). Owns one TCP
/// listener's bookkeeping plus per-connection state. The dispatcher holds
/// this as the cap's state and routes envelopes through the macro-emitted
/// `Dispatch` impl; the addressing identity is the distinct ZST
/// [`RpcServerCapability`]. Living in this
/// private module keeps it `pub`-enough to satisfy the `NativeActor::State`
/// interface without exposing it as crate-public API; fields are
/// `pub` so the parent's handlers / `init` / `unwire` reach them.
pub struct RpcServerState {
    pub peer_kind: PeerKind,
    /// Wakes this cap from the accept and reader threads so the next
    /// `on_inbound_ready` turn drains the inbound channel. Each thread holds
    /// a clone; none holds this cap's mailbox position.
    pub wake: SelfWake<RpcInboundReady>,
    /// Engine → the proxy registered for it: the call path's lookup for
    /// an `engine = Some(_)` `Call`. Kept in step with
    /// [`Self::route_owners`]; neither map is ever scanned.
    pub engine_routes: HashMap<EngineId, ErasedActorRef>,
    /// Registrant → its route: the notice path's lookup when a registered
    /// proxy departs, and the owner of its in-flight correlations. Kept in
    /// step with [`Self::engine_routes`]; neither map is ever scanned.
    pub route_owners: HashMap<ErasedActorRef, RouteOwner>,
    /// Cached `Arc<Mailer>` for the `Call` dispatcher's settlement
    /// subscription: it reads the chassis settlement registry and passes
    /// the same Arc into `subscribe_settlement_mail`. Init grabs it from
    /// `NativeInitCtx::mailer()`; the cap is single-threaded post-ADR-0038
    /// so direct storage is fine.
    pub mailer: Arc<Mailer>,
    /// The bound address, or `None` when the cap was composed disabled
    /// (ADR-0155 §3): a disabled server claims its mailbox but never
    /// binds, so there is no address to reconnect for teardown and no
    /// accept thread to unblock.
    pub bind_addr: Option<String>,
    pub listener_port: u16,
    pub accept_shutdown: Arc<AtomicBool>,
    pub accept_thread: Option<JoinHandle<()>>,
    pub inbound_rx: mpsc::Receiver<InboundEvent>,
    pub inbound_tx: mpsc::Sender<InboundEvent>,
    pub connections: HashMap<ConnId, ConnState>,
    pub next_conn_id: ConnId,
    /// Internal-correlation → connection / wire-cid. Populated on
    /// `Call { cid: Some(n) }` dispatch; cleared on settlement.
    pub in_flight: HashMap<u64, InFlight>,
}

impl RpcServerState {
    /// A state with no listener: every map empty, no bound address, no
    /// accept thread. `init` starts from this for every mode; a bound
    /// server then starts accepting through [`Self::start_accepting`].
    fn unbound(peer_kind: PeerKind, wake: SelfWake<RpcInboundReady>, mailer: Arc<Mailer>) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
        Self {
            peer_kind,
            wake,
            engine_routes: HashMap::new(),
            route_owners: HashMap::new(),
            mailer,
            bind_addr: None,
            listener_port: 0,
            accept_shutdown: Arc::new(AtomicBool::new(false)),
            accept_thread: None,
            inbound_rx,
            inbound_tx,
            connections: HashMap::new(),
            next_conn_id: 0,
            in_flight: HashMap::new(),
        }
    }

    /// Start accepting on a bound `listener`: spawn the
    /// `aether-rpc-accept-{port}` thread and record the bound address, port
    /// and thread so `unwire` tears it down. `init` calls this under
    /// [`RpcBind::Boot`]; the `Bound` arm of `on_inbound_ready` calls it when
    /// a held server's gate opens. Returns the bound port.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener's address cannot be read, it cannot
    /// be made blocking, or the accept thread cannot be spawned. Nothing is
    /// recorded then, so `unwire` has nothing to join.
    pub fn start_accepting(&mut self, listener: TcpListener) -> io::Result<u16> {
        let local_addr = listener.local_addr()?;
        let port = local_addr.port();
        listener.set_nonblocking(false)?;

        let accept_shutdown = Arc::clone(&self.accept_shutdown);
        let inbound_tx = self.inbound_tx.clone();
        let wake = self.wake.clone();

        // Transport thread below the mail layer — it accepts sockets that carry
        // inbound mail in; no inbound chain to inherit, no settlement umbrella.
        let thread = self.wake.spawn_sidecar(format!("aether-rpc-accept-{port}"), move || {
            while !accept_shutdown.load(Ordering::Acquire) {
                if let Ok((stream, peer)) = listener.accept() {
                    if accept_shutdown.load(Ordering::Acquire) {
                        drop(stream);
                        break;
                    }
                    if inbound_tx.send(InboundEvent::PeerAccepted { stream, peer }).is_err() {
                        break;
                    }
                    wake.wake(&RpcInboundReady::default());
                } else if accept_shutdown.load(Ordering::Acquire) {
                    break;
                }
            }
        })?;

        self.bind_addr = Some(local_addr.to_string());
        self.listener_port = port;
        self.accept_thread = Some(thread);
        tracing::info!(
            target: "aether_substrate::rpc",
            addr = %local_addr,
            port = port,
            "rpc server bound",
        );
        Ok(port)
    }

    /// Remove one in-flight call, and its correlation from the owner's
    /// [`RouteOwner::calls`] when it was forwarded. Both are keyed
    /// lookups.
    pub fn take_in_flight(&mut self, correlation: u64) -> Option<InFlight> {
        let entry = self.in_flight.remove(&correlation)?;
        if let Some(owner) = entry.route.and_then(|route| self.route_owners.get_mut(&route)) {
            owner.calls.remove(&correlation);
        }
        Some(entry)
    }

    /// Record `sender` as the route for `engine` (the five cases of
    /// `on_register_engine_route`). Every answer is one keyed lookup in
    /// one of the two route maps.
    pub fn register_engine_route<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        sender: ErasedActorRef,
        engine: EngineId,
    ) -> RegisterEngineRouteResult {
        if let Some(holder) = self.engine_routes.get(&engine) {
            if *holder == sender {
                return RegisterEngineRouteResult::Ok;
            }
            return RegisterEngineRouteResult::Err {
                error: format!("engine {} already has a registered route", engine.0),
            };
        }
        if let Some(owned) = self.route_owners.get(&sender) {
            return RegisterEngineRouteResult::Err {
                error: format!(
                    "cannot register engine {}: this registrant already routes engine {}",
                    engine.0, owned.engine.0,
                ),
            };
        }

        let monitor = match ctx.monitor(sender) {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(
                    target: "aether_substrate::rpc",
                    engine = %engine.0,
                    ?error,
                    "engine route registrant is not monitorable; its route cannot be retired when it departs",
                );
                None
            }
        };
        self.engine_routes.insert(engine, sender);
        self.route_owners.insert(sender, RouteOwner { engine, calls: HashSet::new(), _monitor: monitor });
        RegisterEngineRouteResult::Ok
    }

    /// Allocate a fresh `ConnId`, store the connection's write half,
    /// spin a reader thread for the read half.
    pub fn spawn_reader_for_peer<A>(&mut self, _ctx: &mut NativeCtx<'_, A>, stream: TcpStream, peer: SocketAddr) {
        let conn_id = self.next_conn_id;
        self.next_conn_id += 1;

        let read_half = match stream.try_clone() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::rpc",
                    peer = %peer,
                    error = %e,
                    "rpc conn: try_clone failed; dropping",
                );
                return;
            }
        };
        let write_half = stream;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);

        let wake = self.wake.clone();
        let inbound_tx = self.inbound_tx.clone();

        // Per-connection transport reader below the mail layer — carries inbound
        // mail in; no inbound chain to inherit, no settlement umbrella.
        let thread = match self.wake.spawn_sidecar(format!("aether-rpc-reader-{conn_id}"), move || {
            run_reader_loop(read_half, conn_id, &shutdown_for_thread, &inbound_tx, &wake);
        }) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::rpc",
                    peer = %peer,
                    error = %e,
                    "rpc reader thread spawn failed",
                );
                return;
            }
        };

        self.connections.insert(
            conn_id,
            ConnState { peer, write_half, shutdown, reader_thread: Some(thread), hello_received: false },
        );
        tracing::debug!(
            target: "aether_substrate::rpc",
            conn = conn_id,
            peer = %peer,
            "rpc conn accepted",
        );
    }

    /// Dispatch one incoming frame.
    pub fn dispatch_frame<A: HandlesKind<Settled>>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        conn_id: ConnId,
        frame: WireFrame,
    ) {
        match frame {
            WireFrame::Hello(hello) => self.handle_hello(conn_id, hello),
            WireFrame::HelloAck(_) => {
                // Server doesn't expect HelloAck — only clients do.
                tracing::debug!(
                    target: "aether_substrate::rpc",
                    conn = conn_id,
                    "received HelloAck on server side; ignoring",
                );
            }
            WireFrame::Call { cid, envelope } => self.handle_call(ctx, conn_id, cid, envelope),
            WireFrame::ReplyEvent { .. } | WireFrame::ReplyEnd { .. } => {
                // Server doesn't expect reply frames inbound.
                tracing::debug!(
                    target: "aether_substrate::rpc",
                    conn = conn_id,
                    "received reply frame on server side; ignoring",
                );
            }
            WireFrame::Ping(token) => {
                self.write_frame_to(conn_id, &WireFrame::Pong(token));
            }
            WireFrame::Pong(_) => {
                // Cap doesn't initiate Pings v1; nothing to track.
            }
            WireFrame::Bye { reason } => {
                self.close_connection(conn_id, &format!("peer bye: {reason}"));
            }
        }
    }

    pub fn handle_hello(&mut self, conn_id: ConnId, hello: Hello) {
        if hello.wire_version != WIRE_VERSION {
            self.write_frame_to(
                conn_id,
                &WireFrame::Bye {
                    reason: format!("wire_version mismatch: peer={}, server={WIRE_VERSION}", hello.wire_version),
                },
            );
            self.close_connection(conn_id, "wire_version mismatch");
            return;
        }
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            conn.hello_received = true;
        }
        self.write_frame_to(
            conn_id,
            &WireFrame::HelloAck(HelloAck { wire_version: WIRE_VERSION, server: self.peer_kind.clone() }),
        );
    }

    pub fn handle_call<A: HandlesKind<Settled>>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        conn_id: ConnId,
        cid: Option<u64>,
        envelope: MailEnvelope,
    ) {
        // ADR-0233: engine-only mail never arrives from the wire. Refused
        // first, so neither a forward to another engine nor a proven local
        // dispatch carries it; the reason names the kind by its tagged id.
        if is_engine_only(envelope.kind) {
            let reason = format!("{} is engine-only mail", envelope.kind);
            let Some(wire_cid) = cid else {
                tracing::warn!(target: "aether_substrate::rpc", conn = conn_id, %reason, "rpc call refused");
                return;
            };
            self.write_frame_to(
                conn_id,
                &WireFrame::ReplyEnd { cid: wire_cid, result: Err(RpcError::Other { reason }) },
            );
            return;
        }
        // The envelope names an engine (issue 763 P5a): relay to the
        // proxy registered for it, as a `ForwardEnvelope`. This server is
        // the sender, so the send's default reply target is this server
        // under the minted correlation: the substrate's reply streams back
        // through the proxy as a normal reply mail (handled by `on_any` as
        // a `ReplyEvent`), and its terminal `ReplyEnd` arrives — via the
        // proxy — as a `CallSettled` (also handled by `on_any`).
        //
        // Crucially this path does NOT subscribe to settlement: the
        // local `ForwardEnvelope` chain settles almost immediately,
        // long before the remote substrate replies, so settlement
        // would close the wire call prematurely. The terminal close
        // comes from `CallSettled`, or from the proxy's `MonitorNotice`
        // if it departs first.
        if let Some(engine) = envelope.to.engine {
            let Some(route) = self.engine_routes.get(&engine).copied() else {
                let Some(wire_cid) = cid else {
                    tracing::debug!(
                        target: "aether_substrate::rpc",
                        conn = conn_id,
                        engine = %engine.0,
                        "rpc forward requested for an engine with no route; dropping",
                    );
                    return;
                };
                self.write_frame_to(
                    conn_id,
                    &WireFrame::ReplyEnd { cid: wire_cid, result: Err(RpcError::UnknownEngine { engine }) },
                );
                return;
            };
            let forward =
                ForwardEnvelope { recipient: envelope.to.path, kind: envelope.kind, payload: envelope.payload };
            let mail_id =
                ctx.send_envelope_detached_to(route, <ForwardEnvelope as Kind>::ID, &forward.encode_into_bytes());
            if let Some(wire_cid) = cid {
                let correlation = mail_id.correlation_id;
                self.in_flight.insert(correlation, InFlight { conn_id, wire_cid, route: Some(route) });
                if let Some(owner) = self.route_owners.get_mut(&route) {
                    owner.calls.insert(correlation);
                }
            }
            return;
        }
        // Resolve and prove the recipient's `ActorPath` on arrival
        // (ADR-0230 section 3): this engine hosts it, so only this engine
        // expands a short path, and nothing upstream computed a position for
        // it. `accept_call` hands back an item that can only be delivered, so
        // this server holds no proof it could keep. A path that does not
        // resolve to a `Live` actor dispatches nothing and closes the call
        // with `RpcError::NotPresent`, whatever the reason, rather than
        // parking or dropping in the mailer.
        let item = match ctx.accept_call(&envelope.to.path, envelope.kind, envelope.payload) {
            Ok(item) => item,
            Err(detail) => {
                let path = envelope.to.path;
                let Some(wire_cid) = cid else {
                    tracing::warn!(
                        target: "aether_substrate::rpc",
                        conn = conn_id,
                        %path,
                        %detail,
                        "rpc call refused: recipient is not present",
                    );
                    return;
                };
                self.write_frame_to(
                    conn_id,
                    &WireFrame::ReplyEnd { cid: wire_cid, result: Err(RpcError::NotPresent { path, detail }) },
                );
                return;
            }
        };

        // Dispatch the item as a fresh chain. The returned MailId is the
        // new chain's root; if cid is Some, subscribe to its settlement to
        // know when to write ReplyEnd.
        let mail_id = ctx.deliver_detached(item);

        let Some(wire_cid) = cid else {
            // Fire-and-forget at the wire layer. No bookkeeping.
            return;
        };

        // Subscribe to settlement of the dispatched chain so we
        // close the call with a ReplyEnd. Requires the chassis
        // settlement registry — fail loud if not wired.
        if !ctx.subscribe_settlement::<Settled>(mail_id) {
            self.write_frame_to(
                conn_id,
                &WireFrame::ReplyEnd {
                    cid: wire_cid,
                    result: Err(RpcError::Other { reason: "settlement registry unavailable on this chassis".into() }),
                },
            );
            return;
        }
        self.in_flight.insert(mail_id.correlation_id, InFlight { conn_id, wire_cid, route: None });
    }

    pub fn close_connection(&mut self, conn_id: ConnId, reason: &str) {
        let Some(mut conn) = self.connections.remove(&conn_id) else {
            return;
        };
        conn.shutdown.store(true, Ordering::Release);
        let _ = conn.write_half.shutdown(Shutdown::Both);
        // Drop reader_thread without joining inline — the
        // dispatcher must not block on the reader. The thread sees
        // the shutdown flag (or its own EOF) and exits; the
        // JoinHandle drop detaches.
        drop(conn.reader_thread.take());
        // Clear in-flight entries pinned to this connection so we
        // don't write ReplyEvents / ReplyEnds to a dead socket. A dropped
        // forwarded call also leaves its owner's `calls`, so a long-lived
        // proxy accumulates no stale correlations.
        let route_owners = &mut self.route_owners;
        self.in_flight.retain(|correlation, entry| {
            if entry.conn_id != conn_id {
                return true;
            }
            if let Some(owner) = entry.route.and_then(|route| route_owners.get_mut(&route)) {
                owner.calls.remove(correlation);
            }
            false
        });
        tracing::debug!(
            target: "aether_substrate::rpc",
            conn = conn_id,
            peer = %conn.peer,
            reason,
            "rpc conn closed",
        );
    }

    pub fn write_frame_to(&mut self, conn_id: ConnId, frame: &WireFrame) {
        let Some(conn) = self.connections.get_mut(&conn_id) else {
            return;
        };
        if let Err(e) = write_frame(&mut conn.write_half, frame) {
            let reason = match &e {
                FrameError::Io(io_err)
                    if matches!(
                        io_err.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::WriteZero
                    ) =>
                {
                    "peer hung up"
                }
                FrameError::Io(_) => "write error",
                _ => "frame encode error",
            };
            tracing::debug!(
                target: "aether_substrate::rpc",
                conn = conn_id,
                error = %e,
                "rpc frame write failed",
            );
            self.close_connection(conn_id, reason);
        }
    }
}

#[runtime]
impl NativeActor for RpcServerCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// state-bearing struct holding the TCP listener bookkeeping +
    /// per-connection state.
    type State = RpcServerState;
    type Config = RpcServerConfig;
    type Params = RpcServerParams;
    const NAMESPACE: &'static str = "aether.rpc.server";

    fn init(
        config: RpcServerConfig,
        params: RpcServerParams,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<RpcServerState, BootError> {
        let mut state = RpcServerState::unbound(params.peer_kind, ctx.self_wake::<RpcInboundReady>(), ctx.mailer());

        // ADR-0155 §3: the cap is always composed and always claims its
        // mailbox; the resolved port gates only what Start does. A `None`
        // port is the disabled state — claim the mailbox, bind no socket,
        // spawn no accept thread, publish no handle. Mail arriving here is
        // then answered (or intercepted by `on_any`) rather than warn-dropped
        // at an unknown mailbox. `Some(port)` binds localhost (single-host
        // development story); `0` lets the OS pick an ephemeral port.
        let Some(bind_port) = config.port else {
            tracing::info!(
                target: "aether_substrate::rpc",
                "rpc server composed disabled (no bind port); claiming mailbox, binding no socket",
            );
            return Ok(state);
        };

        match params.bind {
            RpcBind::Boot => {
                let port = TcpListener::bind(("127.0.0.1", bind_port))
                    .and_then(|listener| state.start_accepting(listener))
                    .map_err(|e| BootError::Other(Box::new(e)))?;
                if let Some(path) = &config.port_file {
                    write_port_file(Path::new(path), port).map_err(|e| BootError::Other(Box::new(e)))?;
                }
                ctx.publish_handle(RpcServerHandle { local_port: port });
            }
            // Issue #6399: bind nothing yet. The composer opens the gate once
            // the engine is ready; its `Bound` event reaches `on_inbound_ready`,
            // which starts accepting through the same helper.
            RpcBind::Held => {
                tracing::info!(
                    target: "aether_substrate::rpc",
                    port = bind_port,
                    "rpc server composed held; binding when its gate opens",
                );
                ctx.publish_handle(RpcBindGate {
                    port: bind_port,
                    port_file: config.port_file.map(PathBuf::from),
                    inbound: state.inbound_tx.clone(),
                    wake: state.wake.clone(),
                    opened: Arc::new(AtomicBool::new(false)),
                });
            }
        }
        Ok(state)
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // A disabled server (ADR-0155 §3), or a held one whose gate never
        // opened, bound no socket and spawned no accept thread, so there is
        // nothing to unblock or join.
        let Some(bind_addr) = state.bind_addr.clone() else {
            return;
        };
        // Stop the accept thread; self-connect to unblock its blocking
        // `accept()`.
        state.accept_shutdown.store(true, Ordering::Release);
        let wake_addr = teardown_connect_addr(&bind_addr, state.listener_port);
        if let Err(error) = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(100)) {
            tracing::warn!(
                target: "aether_substrate::rpc",
                port = state.listener_port,
                addr = %wake_addr,
                %error,
                "rpc server teardown wake self-connect failed; accept-thread join may stall",
            );
        }
        if let Some(t) = state.accept_thread.take() {
            let _ = t.join();
        }
        // Stop every per-connection reader. Shutting down the read
        // half wakes the blocked `read()`; the reader sees the
        // shutdown flag and exits.
        for conn in state.connections.values_mut() {
            conn.shutdown.store(true, Ordering::Release);
            let _ = conn.write_half.shutdown(Shutdown::Read);
            if let Some(t) = conn.reader_thread.take() {
                let _ = t.join();
            }
        }
        tracing::info!(
            target: "aether_substrate::rpc",
            port = state.listener_port,
            "rpc server closed",
        );
    }

    /// Sidecar wake. Drain every pending inbound event.
    ///
    /// # Agent
    /// Internal wake mail — not part of the cap's external surface.
    /// The accept / reader sidecars fire this to wake the
    /// dispatcher; the handler drains the mpsc and dispatches per
    /// item.
    #[handler::single]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self>, _mail: RpcInboundReady) {
        while let Ok(event) = state.inbound_rx.try_recv() {
            match event {
                InboundEvent::Bound { listener } => {
                    if state.bind_addr.is_some() {
                        tracing::warn!(
                            target: "aether_substrate::rpc",
                            port = state.listener_port,
                            "rpc server already bound; dropping the second listener",
                        );
                        continue;
                    }
                    if let Err(error) = state.start_accepting(listener) {
                        tracing::error!(
                            target: "aether_substrate::rpc",
                            %error,
                            "rpc server could not start accepting on its opened gate; dials will be refused",
                        );
                    }
                }
                InboundEvent::PeerAccepted { stream, peer } => {
                    state.spawn_reader_for_peer(ctx, stream, peer);
                }
                InboundEvent::FrameReceived { conn_id, frame } => {
                    state.dispatch_frame(ctx, conn_id, frame);
                }
                InboundEvent::ReaderClosed { conn_id, reason } => {
                    state.close_connection(conn_id, &reason);
                }
                InboundEvent::FrameDecodeError { conn_id, error } => {
                    // The reader kept frame-sync (body drained).
                    // Write a structured `ReplyEnd { cid: 0, Err }`
                    // and leave the connection up so further calls
                    // on this socket still work (issue 1271).
                    //
                    // `cid = 0` is the sentinel: the wire couldn't
                    // be decoded far enough to learn the real cid,
                    // so we report against id 0 and the mcp router
                    // surfaces it as a wire-level out-of-band
                    // failure rather than a per-call settled-Err.
                    tracing::warn!(
                        target: "aether_substrate::rpc",
                        conn = conn_id,
                        error = ?error,
                        "rpc inbound frame decode error; keeping connection alive",
                    );
                    state.write_frame_to(conn_id, &WireFrame::ReplyEnd { cid: 0, result: Err(error) });
                }
                InboundEvent::FrameDecodeAborted { conn_id, error } => {
                    // The announced body was big enough to be its
                    // own OOM vector (size > 2 * max). Write a
                    // structured `Bye` so the peer sees a named
                    // close instead of a bare reset, then tear the
                    // connection down (issue 1271).
                    let reason = match &error {
                        RpcError::FrameTooLarge { size, max } => {
                            format!("frame too large: {size} > {max}")
                        }
                        other => format!("frame decode aborted: {other:?}"),
                    };
                    tracing::warn!(
                        target: "aether_substrate::rpc",
                        conn = conn_id,
                        reason = %reason,
                        "rpc inbound frame too large to drain; closing connection",
                    );
                    state.write_frame_to(conn_id, &WireFrame::Bye { reason: reason.clone() });
                    state.close_connection(conn_id, &reason);
                }
            }
        }
    }

    /// Settlement notice from the chassis. The root corresponds
    /// to a `Call` dispatch we subscribed to; close the call by
    /// writing `ReplyEnd { cid, result: Ok(()) }` and dropping
    /// the in-flight entry.
    ///
    /// # Agent
    /// Internal — fires from `SettlementRegistry::fire_settled`,
    /// not from external mail. Subscribers parked in the registry
    /// receive one of these per settled root.
    #[handler::single]
    fn on_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Settled) {
        let correlation = mail.root.correlation_id;
        let Some(entry) = state.take_in_flight(correlation) else {
            // No matching in-flight call. Either we never owned
            // this root or the connection already closed and we
            // cleared eagerly. Either way: drop silently.
            return;
        };
        state.write_frame_to(entry.conn_id, &WireFrame::ReplyEnd { cid: entry.wire_cid, result: Ok(()) });
    }

    /// Register the sending proxy as the route for one engine.
    ///
    /// # Agent
    /// Internal — a per-engine proxy sends `RegisterEngineRoute {
    /// engine_id }` from its `wire` hook, for its own engine. The
    /// registrant is the envelope sender, so a mail with no local sender
    /// is refused. An engine already routed to a different registrant
    /// is refused and the holder keeps it; a registrant that already
    /// routes a different engine is refused; the same registrant
    /// re-registering its own engine is `Ok` and changes nothing.
    /// Reply: `RegisterEngineRouteResult`.
    #[handler::single]
    fn on_register_engine_route(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: RegisterEngineRoute,
    ) -> RegisterEngineRouteResult {
        let Some(sender) = ctx.sender() else {
            return RegisterEngineRouteResult::Err {
                error: format!("cannot register engine {}: the registration has no local sender", mail.engine_id.0),
            };
        };
        state.register_engine_route(ctx, sender, mail.engine_id)
    }

    /// Retire a departed registrant's engine route (ADR-0079 §8 amended).
    ///
    /// The host stamps the departed registrant as the notice's sender, so
    /// `ctx.sender()` is the same proven reference [`RpcServerState::route_owners`]
    /// is keyed by (ADR-0230). The work is proportional to what the notice
    /// retires: the owner row, its engine's route, and each call still in
    /// flight at it, which closes with `ReplyEnd` `Err`. A notice with no
    /// sender, or from an actor that holds no route, changes nothing.
    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        let Some(departed) = ctx.sender() else {
            return;
        };
        let Some(owner) = state.route_owners.remove(&departed) else {
            return;
        };
        state.engine_routes.remove(&owner.engine);
        for correlation in owner.calls {
            let Some(entry) = state.in_flight.remove(&correlation) else {
                continue;
            };
            let reason = format!("engine {} left before the call settled", owner.engine.0);
            state.write_frame_to(
                entry.conn_id,
                &WireFrame::ReplyEnd { cid: entry.wire_cid, result: Err(RpcError::Other { reason }) },
            );
        }
    }

    /// Catch-all. Any mail addressed at this cap that's not one of
    /// the typed wake / settlement kinds is treated as a reply
    /// mail from a downstream actor; if its `correlation_id`
    /// matches an in-flight call, the cap wraps it as a
    /// `ReplyEvent` and writes to the originating connection.
    ///
    /// # Agent
    /// Not user-callable — this is the cap's reply interception
    /// path. The wire is mail-shaped (issue 750 §wire), so any
    /// kind two peers share is reachable; reply correlation goes
    /// through this fallback.
    #[fallback]
    fn on_any(state: &mut Self::State, ctx: &mut NativeCtx<'_>, env: &Envelope) {
        let correlation = env.sender.correlation_id;
        let Some(entry) = state.in_flight.get(&correlation).copied() else {
            tracing::debug!(
                target: "aether_substrate::rpc",
                kind = %ctx.kind_label(env.kind),
                correlation,
                "rpc reply with no matching in-flight call; dropping",
            );
            return;
        };

        // A forwarded engine call (issue 763 P5a) closes when its
        // proxy lifts the substrate's terminal `ReplyEnd` into a
        // `CallSettled` — there's no local chain for `on_settled`
        // to catch. Recognize it here, write the wire `ReplyEnd`,
        // and clear the in-flight entry.
        if env.kind == <CallSettled as Kind>::ID {
            let result = match CallSettled::decode_from_bytes(env.payload.bytes()) {
                Some(CallSettled::Ok) => Ok(()),
                Some(CallSettled::Err { error }) => Err(error),
                None => Err(RpcError::Other { reason: "malformed CallSettled payload".into() }),
            };
            state.take_in_flight(correlation);
            state.write_frame_to(entry.conn_id, &WireFrame::ReplyEnd { cid: entry.wire_cid, result });
            return;
        }

        let envelope = ReplyEnvelope { kind: env.kind, payload: env.payload.bytes().to_vec() };
        state.write_frame_to(entry.conn_id, &WireFrame::ReplyEvent { cid: entry.wire_cid, envelope });
    }
}
