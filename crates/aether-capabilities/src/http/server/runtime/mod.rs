//! The `aether.http.server` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `HttpServerCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` in the parent (and
//! the shard's, in `super::shard::runtime`) reach the state, ctx, and helper
//! types through a single `use …::*` glob.
//!
//! Post-ADR-0135 this module hosts *both* halves of the sharded cap: the
//! supervisor's `#[runtime] impl` below (listener + accept thread, shard
//! spawn/assignment, the ADR-0130 route-registration surface over the shared
//! table) and, in the concern submodules, the whole per-connection machine
//! the dispatch shards run — [`HttpShardState`] and its reader/writer
//! sidecars, parse/render, streaming, and websocket support. The sidecar
//! threads capture only `Arc` / channel / id clones — never an actor struct —
//! so the supervisor/shard split does not change what any thread captures.

// `#[handler]` methods take their decoded payload by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the decoded bytes so
// callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Parent-level items this module names. `HttpServerConfig` is named by
// `init`'s signature, `HttpServerCapability` is the impl's `Self` type, and
// `HttpServerHandle` is the boot artifact `init` publishes.
use super::{HttpInboundReady, HttpServerCapability, HttpServerConfig, HttpServerHandle};
use aether_actor::runtime;

pub use std::collections::HashMap;
pub use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
pub use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
pub use std::sync::{Arc, RwLock, mpsc};
pub use std::thread;
pub use std::time::Duration;

pub use aether_data::{Kind, KindId, MailboxId};
pub use aether_substrate::actor::native::envelope::Envelope;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::MailId;
pub use aether_substrate::mail::mailer::Mailer;
pub use aether_substrate::mail::registry::{MailboxEntry, Registry};

// The shard's `#[runtime] impl` (super::shard::runtime) reaches the kind
// vocabulary its moved handler bodies name through this module's glob, so
// the kinds the shard shares with the concern submodules stay `pub use`.
pub use crate::http::kinds::{
    HttpHeader, HttpMethod, HttpRequestChunk, HttpRequestCredit, HttpRequestStreamEnd, HttpRequestStreamOpen,
    HttpResponseChunk, HttpResponseStreamEnd, HttpResponseStreamOpen, HttpServerRequest, HttpServerResponse,
    HttpStreamCredit, WebSocketAccept, WebSocketClose, WebSocketMessage,
};
use crate::http::kinds::{
    RegisterRoute, RegisterRouteResult, RegisterRouteSelf, UnregisterRoute, UnregisterRouteSelf, UnregisterRoutesAll,
};
use aether_kinds::MonitorNotice;
pub use aether_kinds::trace::Settled;
// `state.rs` reaches `MonitorHandle` through the module-root glob like the
// rest of the substrate surface, so the re-export rides `pub use`.
pub use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::net::teardown_connect_addr;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

pub use aether_substrate::Mail;
use std::io::{self, Read, Write};
use std::mem;
use std::str::from_utf8;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

mod reader;
mod render;
mod routing;
mod state;
mod streaming;
mod types;
mod websocket;

pub use reader::*;
pub use render::*;
pub use routing::*;
pub use state::*;
pub use types::*;
pub use websocket::*;

#[cfg(test)]
mod unit_tests;

#[runtime]
impl NativeActor for HttpServerCapability {
    /// The runtime state this identity boots into (ADR-0122 split,
    /// ADR-0135): the listener port, the accept thread, the shared route
    /// table, and the dispatch-shard sinks. The per-connection machine
    /// lives in the shards ([`HttpShardState`]).
    type State = HttpSupervisorState;

    type Config = HttpServerConfig;

    const NAMESPACE: &'static str = "aether.http.server";

    fn init(config: HttpServerConfig, ctx: &mut NativeInitCtx<'_>) -> Result<HttpSupervisorState, BootError> {
        let listener = TcpListener::bind(&config.bind_addr).map_err(|e| BootError::Other(Box::new(e)))?;
        let local_addr = listener.local_addr().map_err(|e| BootError::Other(Box::new(e)))?;
        let port = local_addr.port();
        listener.set_nonblocking(false).map_err(|e| BootError::Other(Box::new(e)))?;

        let accept_shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown_for_thread = Arc::clone(&accept_shutdown);

        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
        let mailer: Arc<Mailer> = ctx.mailer();
        let self_id = ctx.self_id();
        let wake_kind = <HttpInboundReady as Kind>::ID;

        let wake_dirty = Arc::new(AtomicBool::new(false));
        let accept_sink =
            WakeSink { inbound_tx, mailer: Arc::clone(&mailer), self_id, wake_kind, dirty: Arc::clone(&wake_dirty) };

        // Transport thread below the mail layer — it accepts sockets
        // that carry inbound mail in; no inbound chain to inherit, no
        // settlement umbrella.
        #[allow(clippy::disallowed_methods)]
        let accept_thread = thread::Builder::new()
            .name(format!("aether-http-accept-{port}"))
            .spawn(move || {
                while !accept_shutdown_for_thread.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            if accept_shutdown_for_thread.load(Ordering::Acquire) {
                                drop(stream);
                                break;
                            }
                            if !accept_sink.post(InboundEvent::PeerAccepted { stream, peer }) {
                                break;
                            }
                        }
                        Err(error) => {
                            if accept_shutdown_for_thread.load(Ordering::Acquire) {
                                break;
                            }
                            tracing::warn!(
                                target: "aether_substrate::http_server",
                                port,
                                %error,
                                "http accept() failed; backing off before retry",
                            );
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        tracing::info!(
            target: "aether_substrate::http_server",
            addr = %config.bind_addr,
            port,
            "http server bound",
        );

        ctx.publish_handle(HttpServerHandle { local_port: port });

        Ok(HttpSupervisorState {
            config,
            routes: Arc::new(RwLock::new(Vec::new())),
            live_connections: Arc::new(AtomicUsize::new(0)),
            mailer,
            listener_port: port,
            accept_shutdown,
            accept_thread: Some(accept_thread),
            inbound_rx,
            wake_dirty,
            shards: Vec::new(),
            next_shard: 0,
            next_stream_id: Arc::new(AtomicU64::new(0)),
            monitors: HashMap::new(),
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // Stop the accept thread; self-connect to unblock its blocking
        // `accept()`. The shards join their own reader/writer sidecars in
        // their own `unwire` (the chassis tears instanced actors down
        // alongside the caps).
        state.accept_shutdown.store(true, Ordering::Release);
        let wake_addr = teardown_connect_addr(&state.config.bind_addr, state.listener_port);
        if let Err(error) = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(100)) {
            tracing::warn!(
                target: "aether_substrate::http_server",
                port = state.listener_port,
                addr = %wake_addr,
                %error,
                "http server teardown wake self-connect failed; accept-thread join may stall",
            );
        }
        if let Some(thread) = state.accept_thread.take() {
            let _ = thread.join();
        }
        tracing::info!(
            target: "aether_substrate::http_server",
            port = state.listener_port,
            "http server closed",
        );
    }

    /// Sidecar wake. Drain every pending accepted connection and assign
    /// each to a dispatch shard (ADR-0135).
    ///
    /// # Agent
    /// Internal wake mail — not part of the cap's external surface. The
    /// accept sidecar fires this; the handler drains the mpsc and assigns
    /// per item.
    #[handler::single]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: HttpInboundReady) {
        WakeSink::arm_for_drain(&state.wake_dirty);
        while let Ok(event) = state.inbound_rx.try_recv() {
            match event {
                InboundEvent::PeerAccepted { stream, peer } => {
                    state.assign_peer(ctx, stream, peer);
                }
                // Only the accept thread feeds the supervisor's channel;
                // every other event species is posted by a shard's own
                // sidecars to that shard's channel.
                _ => {
                    tracing::debug!(
                        target: "aether_substrate::http_server",
                        "unexpected non-accept event at supervisor dropped",
                    );
                }
            }
        }
    }

    /// Claim a route for an explicitly named mailbox (ADR-0130).
    ///
    /// # Agent
    /// `RegisterRoute { prefix, method, kind, mailbox }`. The external
    /// form — an MCP session or test names the handler mailbox
    /// explicitly; it is validated against the registry. An in-process
    /// actor registering itself sends `register_route_self` instead.
    #[handler::single]
    fn on_register_route(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: RegisterRoute,
    ) -> RegisterRouteResult {
        if let Err(error) = validate_route_mailbox(state.mailer.registry(), payload.mailbox) {
            return RegisterRouteResult::Err { error };
        }
        let result =
            state.register_route(&payload.prefix, payload.method, payload.kind, payload.mailbox, payload.shared);
        if matches!(result, RegisterRouteResult::Ok) {
            state.watch(ctx, payload.mailbox);
        }
        result
    }

    /// Claim a route for the *sending* actor (ADR-0130), resolved from
    /// the inbound envelope's host-stamped `Source` — forgery-proof
    /// and gated to in-process actors by construction, mirroring
    /// `aether.input.subscribe_self`.
    ///
    /// # Agent
    /// `RegisterRouteSelf { prefix, method, kind }`, typically sent
    /// from a component's `wire` hook. An external session or remote
    /// engine has no local mailbox and gets an `Err` reply — use
    /// `register_route` with an explicit mailbox instead.
    #[handler::single]
    fn on_register_route_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: RegisterRouteSelf,
    ) -> RegisterRouteResult {
        match ctx.source_mailbox() {
            Some(mailbox) => {
                let result =
                    state.register_route(&payload.prefix, payload.method, payload.kind, mailbox, payload.shared);
                if matches!(result, RegisterRouteResult::Ok) {
                    state.watch(ctx, mailbox);
                }
                result
            }
            None => RegisterRouteResult::Err {
                error: "aether.http.server.register_route_self requires a local sender; an \
                        external session or remote engine must use \
                        aether.http.server.register_route with an explicit mailbox"
                    .to_string(),
            },
        }
    }

    /// Release an explicitly named mailbox's route (ADR-0130).
    /// Idempotent.
    ///
    /// # Agent
    /// `UnregisterRoute { prefix, method, mailbox }`.
    #[handler::single]
    fn on_unregister_route(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: UnregisterRoute,
    ) -> RegisterRouteResult {
        state.unregister_route(&payload.prefix, payload.method, payload.mailbox)
    }

    /// Release the *sending* actor's route (ADR-0130), resolved from
    /// the host-stamped `Source` like `register_route_self`.
    /// Idempotent.
    ///
    /// # Agent
    /// `UnregisterRouteSelf { prefix, method }`.
    #[handler::single]
    fn on_unregister_route_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: UnregisterRouteSelf,
    ) -> RegisterRouteResult {
        match ctx.source_mailbox() {
            Some(mailbox) => state.unregister_route(&payload.prefix, payload.method, mailbox),
            None => RegisterRouteResult::Err {
                error: "aether.http.server.unregister_route_self requires a local sender; an \
                        external session or remote engine must use \
                        aether.http.server.unregister_route with an explicit mailbox"
                    .to_string(),
            },
        }
    }

    /// Release every route held by a mailbox (ADR-0130) in one shot.
    /// The externally sendable bulk form — drop-time cleanup happens
    /// through [`Self::on_monitor_notice`] instead, so nothing mails
    /// this on the component path anymore. Idempotent;
    /// fire-and-forget.
    ///
    /// # Agent
    /// `UnregisterRoutesAll { mailbox }`.
    #[handler::single]
    fn on_unregister_routes_all(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, payload: UnregisterRoutesAll) {
        state.unregister_routes_all(payload.mailbox);
    }

    /// Purge a departed mailbox's routes (ADR-0079 §8 amended). The
    /// substrate fires one notice per [`HttpSupervisorState::watch`]ed
    /// mailbox when it vacates (the wasm trampoline on
    /// `DropComponent`) or closes, so the route table stops
    /// dispatching at a dropped trampoline without any drop-time
    /// fan-out from the component host. Releasing the handle keeps the
    /// monitor map bounded by live route holders; a later occupant of
    /// the same mailbox re-registers through its own route claim.
    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        state.monitors.remove(&notice.target);
        state.unregister_routes_all(notice.target);
    }
}
