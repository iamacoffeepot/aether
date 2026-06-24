//! `aether.rpc.server` — generic TCP RPC server capability (issue 750).
//!
//! Singleton actor. Binds a `TcpListener` on the configured addr at
//! init, runs a sidecar accept thread that spawns one reader thread
//! per accepted connection. Reader threads read
//! length-prefix frames via [`aether_codec::frame`] and push them
//! through an internal mpsc; an `RpcInboundReady` wake mail tells the
//! cap's dispatcher to drain.
//!
//! On `Call`, the cap dispatches the wire-borne envelope via
//! `NativeCtx::send_envelope_as_root` (fresh causal chain — the wake
//! mail is causally unrelated to the wire-borne Call) and subscribes
//! to settlement of the resulting root via
//! `SettlementRegistry::subscribe_settlement_mail`. Any reply mail
//! addressed back at this cap with the dispatch's correlation id
//! gets lifted into a `ReplyEvent` and written to the originating
//! connection; the settlement notice closes the call with a
//! `ReplyEnd`.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds need to be importable at file root for the
// `#[actor]`-emitted `HandlesKind<K>` markers (always-on against the
// identity, ADR-0122). `RpcInboundReady` is the cap's own wake-mail kind
// (ADR-0121); `Settled` stays in `aether-kinds`.
use crate::rpc::kinds::RpcInboundReady;
use aether_kinds::trace::Settled;

// Re-export the cap's config at file root for chassis builders. The
// `RpcServerConfig` type names no `aether_substrate` type, so it stays a
// top-level `not(wasm32)` plain struct; the `RpcServerHandle` boot artifact
// lives in the runtime half and is re-exported below under the runtime
// gate.
#[cfg(not(target_arch = "wasm32"))]
mod config;
#[cfg(not(target_arch = "wasm32"))]
pub use config::RpcServerConfig;
#[cfg(feature = "runtime")]
pub use runtime::RpcServerHandle;

// Named at file root so the runtime half reaches it through `super::`
// (`RpcServerState` stores `peer_kind: PeerKind`).
use aether_rpc::rpc::PeerKind;

// The standalone connection plumbing (sidecar event type, per-connection
// state, reader loop, oversize guard) lives in `connection`; the runtime
// half `use`s it. Native-only — it owns a `TcpStream` + OS threads, elided
// on the wasm marker build.
#[cfg(not(target_arch = "wasm32"))]
mod connection;
// `on_inbound_ready` (in the runtime-gated `#[actor] impl` below) matches on
// `InboundEvent` directly, so it is imported here rather than re-exported
// through the `runtime` glob: `InboundEvent` is `pub(super)` in `connection`,
// and a `pub use` of a restricted-visibility item does not glob-export up to
// this module. `feature = "runtime"` implies `not(wasm32)`, so `connection`
// exists wherever this import is active.
#[cfg(feature = "runtime")]
use connection::InboundEvent;

#[cfg(test)]
mod tests;

/// `aether.rpc.server` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable` (`NAMESPACE`,
/// `Resolver`), the per-handler `HandlesKind` markers, and the
/// name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`RpcServerState`, which
/// owns the TCP listener bookkeeping and per-connection state) plus the
/// helper methods live behind the one `feature = "runtime"` gate, so a
/// transport-only build never names `RpcServerState` nor pulls
/// `aether_substrate` through this cap.
pub struct RpcServerCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init/unwire ctxs, the runtime state + `InFlight` bookkeeping,
// the `RpcServerHandle` boot artifact, the per-connection helper methods —
// lives in the `runtime` module below, gated once by `feature = "runtime"`
// and written cfg-free within; the `#[actor] impl` reaches all of it through
// the single `use runtime::*` glob. The kind types (`RpcInboundReady` /
// `Settled`) stay always-on at file root — the always-on `HandlesKind<K>`
// markers name them.
use aether_actor::actor;

// The `runtime` module is this cap's private runtime-half namespace; the impl
// reaches all of it (state, ctx types, helper methods) through this single
// seam, so the glob is intentional rather than a wall of one-line imports.
#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

#[actor(singleton)]
impl NativeActor for RpcServerCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// state-bearing struct holding the TCP listener bookkeeping +
    /// per-connection state.
    type State = RpcServerState;
    type Config = RpcServerConfig;
    const NAMESPACE: &'static str = "aether.rpc.server";

    fn init(
        config: RpcServerConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<RpcServerState, BootError> {
        let listener =
            TcpListener::bind(&config.bind_addr).map_err(|e| BootError::Other(Box::new(e)))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| BootError::Other(Box::new(e)))?;
        let port = local_addr.port();
        listener
            .set_nonblocking(false)
            .map_err(|e| BootError::Other(Box::new(e)))?;

        let accept_shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown_for_thread = Arc::clone(&accept_shutdown);

        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
        let inbound_tx_for_thread = inbound_tx.clone();

        let mailer: Arc<Mailer> = ctx.mailer();
        let self_id = ctx.self_id();
        let wake_kind = KindId(<RpcInboundReady as Kind>::ID.0);

        // Transport thread below the mail layer — it accepts sockets that carry
        // inbound mail in; no inbound chain to inherit, no settlement umbrella.
        #[allow(clippy::disallowed_methods)]
        let thread = thread::Builder::new()
            .name(format!("aether-rpc-accept-{port}"))
            .spawn(move || {
                while !accept_shutdown_for_thread.load(Ordering::Acquire) {
                    if let Ok((stream, peer)) = listener.accept() {
                        if accept_shutdown_for_thread.load(Ordering::Acquire) {
                            drop(stream);
                            break;
                        }
                        if inbound_tx_for_thread
                            .send(InboundEvent::PeerAccepted { stream, peer })
                            .is_err()
                        {
                            break;
                        }
                        mailer.push(Mail::new(
                            self_id,
                            wake_kind,
                            RpcInboundReady::default().encode_into_bytes(),
                            1,
                        ));
                    } else if accept_shutdown_for_thread.load(Ordering::Acquire) {
                        break;
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        tracing::info!(
            target: "aether_substrate::rpc",
            addr = %config.bind_addr,
            port = port,
            "rpc server bound",
        );

        ctx.publish_handle(RpcServerHandle { local_port: port });

        Ok(RpcServerState {
            peer_kind: config.peer_kind,
            self_mailbox: self_id,
            mailer: ctx.mailer(),
            listener_port: port,
            accept_shutdown,
            accept_thread: Some(thread),
            inbound_rx,
            inbound_tx,
            connections: HashMap::new(),
            next_conn_id: 0,
            in_flight: HashMap::new(),
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // Stop the accept thread.
        state.accept_shutdown.store(true, Ordering::Release);
        let addr_str = format!("127.0.0.1:{}", state.listener_port);
        if let Ok(addr) = addr_str.parse::<SocketAddr>() {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
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
    #[handler]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: RpcInboundReady) {
        while let Ok(event) = state.inbound_rx.try_recv() {
            match event {
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
                    state.write_frame_to(
                        conn_id,
                        &WireFrame::ReplyEnd {
                            cid: 0,
                            result: Err(error),
                        },
                    );
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
                    state.write_frame_to(
                        conn_id,
                        &WireFrame::Bye {
                            reason: reason.clone(),
                        },
                    );
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
    #[handler]
    fn on_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Settled) {
        let correlation = mail.root.correlation_id;
        let Some(entry) = state.in_flight.remove(&correlation) else {
            // No matching in-flight call. Either we never owned
            // this root or the connection already closed and we
            // cleared eagerly. Either way: drop silently.
            return;
        };
        state.write_frame_to(
            entry.conn_id,
            &WireFrame::ReplyEnd {
                cid: entry.wire_cid,
                result: Ok(()),
            },
        );
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
    fn on_any(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
        let correlation = env.sender.correlation_id;
        let Some(entry) = state.in_flight.get(&correlation).copied() else {
            tracing::debug!(
                target: "aether_substrate::rpc",
                kind = %env.kind_name,
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
                Some(CallSettled::Err { error }) => Err(RpcError::Other { reason: error }),
                None => Err(RpcError::Other {
                    reason: "malformed CallSettled payload".into(),
                }),
            };
            state.write_frame_to(
                entry.conn_id,
                &WireFrame::ReplyEnd {
                    cid: entry.wire_cid,
                    result,
                },
            );
            state.in_flight.remove(&correlation);
            return;
        }

        let envelope = MailEnvelope {
            to: MailboxAddress::local(state.self_mailbox),
            from: match env.sender.addr {
                SourceAddr::Component(id) => Some(MailboxAddress::local(id)),
                _ => None,
            },
            kind: env.kind,
            correlation_id: Some(entry.wire_cid),
            payload: env.payload.bytes().to_vec(),
        };
        state.write_frame_to(
            entry.conn_id,
            &WireFrame::ReplyEvent {
                cid: entry.wire_cid,
                envelope,
            },
        );
    }
}

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `RpcServerState`, `InFlight`, the `RpcServerHandle` boot artifact, the
// per-connection helper methods) — lives in `runtime.rs`, gated once here.
// The `#[actor] impl` above reaches it through the `use runtime::*` glob.
#[cfg(feature = "runtime")]
mod runtime;
