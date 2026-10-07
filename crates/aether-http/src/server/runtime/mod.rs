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
//! threads capture only `Arc` / channel / self-wake / probe clones — never an
//! actor struct, a mailbox position, or a mailer — so the supervisor/shard
//! split does not change what any thread captures.

// `#[handler]` methods take their decoded payload by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the decoded bytes so
// callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Parent-level items this module names. `HttpServerConfig` is named by
// `init`'s signature, `HttpServerCapability` is the impl's `Self` type, and
// `HttpServerHandle` is the boot artifact `init` publishes.
use super::{HttpDispatchShard, HttpInboundReady, HttpServerCapability, HttpServerConfig, HttpServerHandle};
use aether_actor::{ActorRef, Anyone, ErasedActorRef, PathRefused, ProtocolRef, ReplyMode, Single, runtime};

pub use std::collections::{HashMap, HashSet, VecDeque};
pub use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
pub use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
pub use std::sync::{Arc, RwLock, mpsc};
pub use std::thread;
pub use std::time::Duration;

pub use aether_data::Encoded;
pub use aether_substrate::actor::native::{
    ActorProbe, NativeActor, NativeCtx, NativeInitCtx, SelfWake, SpawnOutcome, TaskDone,
};
pub use aether_substrate::chassis::error::BootError;

// The shard's `#[runtime] impl` (super::shard::runtime) reaches the kind
// vocabulary its moved handler bodies name through this module's glob, so
// the kinds the shard shares with the concern submodules stay `pub use`.
pub use crate::kinds::{
    HttpHeader, HttpMethod, HttpRequestChunk, HttpRequestCredit, HttpRequestStreamEnd, HttpRequestStreamOpen,
    HttpResponseChunk, HttpResponseStreamEnd, HttpResponseStreamOpen, HttpServerRequest, HttpServerResponse,
    HttpStreamCredit, WebSocketAccept, WebSocketClose, WebSocketMessage,
};
use crate::kinds::{
    HttpRouter, RegisterRoute, RegisterRouteResult, RegisterRouteSelf, RequestStreamRouter, StreamCreditRouter,
    UnregisterRoute, UnregisterRouteSelf, WebSocketRouter,
};
use aether_kinds::MonitorNotice;
pub use aether_kinds::trace::Settled;
// `state.rs` reaches `MonitorHandle` through the module-root glob like the
// rest of the substrate surface, so the re-export rides `pub use`.
pub use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::net::teardown_connect_addr;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

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

/// ADR-0155 §3 fail-fast reply for a disabled server. The cap is composed
/// and claims `aether.http.server`, but no socket is bound, so the
/// route-registration surface answers `Err` rather than letting the mail
/// warn-drop at an unknown mailbox — "linked but not enabled" is a
/// first-class, diagnosable state.
fn disabled_route_result() -> RegisterRouteResult {
    RegisterRouteResult::rejected(
        "aether.http.server is composed but disabled on this chassis (enabled = false); \
         no socket is bound, so routes cannot be registered",
    )
}

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
        // ADR-0155 §3: the cap is always composed and always claims its
        // mailbox; the resolved `enabled` flag gates only what Start does.
        // Disabled — claim the mailbox, bind no socket, spawn no accept
        // thread, publish no handle. The route-registration handlers then
        // fail fast with an `Err` reply rather than the mail warn-dropping
        // at an unknown mailbox, and the same binary claims the same
        // namespace wherever `--describe` runs.
        if !config.enabled {
            tracing::info!(
                target: "aether_http::server",
                "http server composed disabled (enabled = false); claiming mailbox, binding no socket",
            );
            return Ok(HttpSupervisorState::disabled(config));
        }

        let listener = TcpListener::bind(&config.bind_addr).map_err(|e| BootError::Other(Box::new(e)))?;
        let local_addr = listener.local_addr().map_err(|e| BootError::Other(Box::new(e)))?;
        let port = local_addr.port();
        listener.set_nonblocking(false).map_err(|e| BootError::Other(Box::new(e)))?;

        let accept_shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown_for_thread = Arc::clone(&accept_shutdown);

        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
        let wake_dirty = Arc::new(AtomicBool::new(false));
        let wake = ctx.self_wake::<HttpInboundReady>();
        let accept_sink = WakeSink { inbound_tx, wake: wake.clone(), dirty: Arc::clone(&wake_dirty) };

        // Transport thread below the mail layer — it accepts sockets
        // that carry inbound mail in; no inbound chain to inherit, no
        // settlement umbrella.
        let accept_thread = wake
            .spawn_sidecar(format!("aether-http-accept-{port}"), move || {
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
                                target: "aether_http::server",
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
            target: "aether_http::server",
            addr = %config.bind_addr,
            port,
            "http server bound",
        );

        ctx.publish_handle(HttpServerHandle { local_port: port });

        Ok(HttpSupervisorState {
            config,
            routes: Arc::new(RwLock::new(RouteTable::default())),
            live_connections: Arc::new(AtomicUsize::new(0)),
            listener_port: port,
            accept_shutdown,
            accept_thread: Some(accept_thread),
            inbound_rx,
            wake_dirty,
            shard_startup: ShardStartup::Idle,
            next_stream_id: Arc::new(AtomicU64::new(0)),
            monitors: HashMap::new(),
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // A disabled server (ADR-0155 §3) bound no socket and spawned no
        // accept thread, so there is nothing to unblock or join.
        if !state.config.enabled {
            return;
        }
        // Stop the accept thread; self-connect to unblock its blocking
        // `accept()`. The shards join their own reader/writer sidecars in
        // their own `unwire` (the chassis tears instanced actors down
        // alongside the caps).
        state.accept_shutdown.store(true, Ordering::Release);
        let wake_addr = teardown_connect_addr(&state.config.bind_addr, state.listener_port);
        if let Err(error) = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(100)) {
            tracing::warn!(
                target: "aether_http::server",
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
            target: "aether_http::server",
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
    #[handler::tell]
    fn on_inbound_ready(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Anyone, Single>,
        _mail: HttpInboundReady,
    ) {
        WakeSink::arm_for_drain(&state.wake_dirty);
        // One deterministic child per handler turn keeps each birth in its
        // own transactional owner batch. A canonical-name conflict can then
        // reject one shard without rolling back its siblings. The private
        // wake scheduled by this step brings the next index (or resumes the
        // accept drain after the final index).
        if state.stage_next_shard(ctx) {
            return;
        }
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
                        target: "aether_http::server",
                        "unexpected non-accept event at supervisor dropped",
                    );
                }
            }
        }
    }

    /// Settle one dispatch-shard birth. A successful child becomes
    /// selectable only from its authoritative `SpawnOutcome`; startup waits
    /// for every deterministic index so completion order cannot reorder the
    /// round-robin set. The birth owes no reply, and the rest of the shard's
    /// sink waits in its startup slot under the index its key names.
    #[handler(task)]
    fn on_shard_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<HttpDispatchShard>>,
    ) {
        let Some(ShardSpawnKey { index }) = ctx.take_context() else {
            return;
        };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let sink = match done.into_output().result {
            Ok(shard) => state.staged_sink(index, shard),
            Err(error) => {
                tracing::warn!(
                    target: "aether_http::server",
                    shard = %shard_subname(index),
                    error = ?error,
                    "http dispatch shard activation failed",
                );
                None
            }
        };

        let settlement = state.finish_shard_spawn(index, sink);
        state.apply_shard_settlement(ctx, settlement);
    }

    /// Claim a route for an explicitly named handler (ADR-0130).
    ///
    /// The handler arrives as a `ProtocolPath<HttpRouter>`, so the contextual
    /// decode already proved that the route at the path, live or closed,
    /// takes `aether.http.server.request` and replies `HttpRouterResult`
    /// (ADR-0231 §3), and a path that did not prove is answered
    /// `Err(Handler(..))` by the dispatch; `resolve` proves it is live,
    /// answering the same `Err` naming the path when its handler has closed,
    /// and the route holds that proof. Its erased twin is the identity the
    /// table, the monitors, and a departure are keyed by. The proof is cast once, here, to each
    /// data-phase protocol ([`RouteMember::cast`]), so the route holds the
    /// references its streams and websockets send through.
    ///
    /// # Agent
    /// `RegisterRoute { prefix, method, handler, shared }`. The external
    /// form — an MCP session or test names the handler by its canonical
    /// path. An in-process actor registering itself sends
    /// `register_route_self` instead.
    #[handler::request]
    fn on_register_route(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: RegisterRoute,
    ) -> RegisterRouteResult {
        if !state.config.enabled {
            return disabled_route_result();
        }
        let handler = match ctx.resolve(&payload.handler) {
            Ok(handler) => handler,
            Err(error) => return PathRefused::from(error).into(),
        };
        let member = RouteMember::cast(ctx, handler);
        let result = state.register_route(&payload.prefix, payload.method, member, payload.shared);
        if matches!(result, RegisterRouteResult::Ok) {
            state.watch(ctx, handler.erase());
        }
        result
    }

    /// Claim a route for the *sending* actor (ADR-0130), resolved from
    /// the inbound envelope's host-stamped `Source` — forgery-proof
    /// and gated to in-process actors by construction, mirroring
    /// `aether.window.subscribe_self`. The sender is cast to `HttpRouter`
    /// once, here, so the route holds the same proof an explicit
    /// registration holds; a sender whose rows do not cover it is refused.
    /// The same [`RouteMember::cast`] then types it for the data phase.
    ///
    /// # Agent
    /// `RegisterRouteSelf { prefix, method, shared }`, typically sent
    /// from a component's `wire` hook. An external session or remote
    /// engine has no local mailbox and gets an `Err` reply — use
    /// `register_route` with an explicit handler path instead.
    #[handler::request]
    fn on_register_route_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: RegisterRouteSelf,
    ) -> RegisterRouteResult {
        if !state.config.enabled {
            return disabled_route_result();
        }
        let Some(sender) = ctx.sender() else {
            return RegisterRouteResult::rejected(
                "aether.http.server.register_route_self requires a local sender; an \
                 external session or remote engine must use \
                 aether.http.server.register_route with an explicit handler path",
            );
        };
        let Some(handler) = ctx.cast::<HttpRouter>(sender) else {
            return RegisterRouteResult::rejected(format!(
                "{} does not cover HttpRouter: a route holder takes aether.http.server.request \
                 and replies aether.http.server.router_result",
                ctx.actor_path(sender),
            ));
        };
        let member = RouteMember::cast(ctx, handler);
        let result = state.register_route(&payload.prefix, payload.method, member, payload.shared);
        if matches!(result, RegisterRouteResult::Ok) {
            state.watch(ctx, sender);
        }
        result
    }

    /// Release an explicitly named handler's route (ADR-0130). Idempotent.
    /// Release needs only the identity the route table is keyed by, and the
    /// server never sends to a holder it is releasing, so the handler is
    /// named by a plain path and proven with `resolve_path` (ADR-0231 §3).
    /// A path that no longer proves holds nothing to release — a monitored
    /// holder's routes already went with its `MonitorNotice` — so the
    /// refusal is the same `Ok` an unheld route gets.
    ///
    /// # Agent
    /// `UnregisterRoute { prefix, method, handler }`. The path may be short;
    /// `resolve_path` expands it.
    #[handler::request]
    fn on_unregister_route(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: UnregisterRoute,
    ) -> RegisterRouteResult {
        if !state.config.enabled {
            return disabled_route_result();
        }
        match ctx.resolve_path(&payload.handler) {
            Ok(holder) => state.unregister_route(&payload.prefix, payload.method, holder),
            Err(_) => RegisterRouteResult::Ok,
        }
    }

    /// Release the *sending* actor's route (ADR-0130), resolved from
    /// the host-stamped `Source` like `register_route_self`.
    /// Idempotent.
    ///
    /// # Agent
    /// `UnregisterRouteSelf { prefix, method }`.
    #[handler::request]
    fn on_unregister_route_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: UnregisterRouteSelf,
    ) -> RegisterRouteResult {
        if !state.config.enabled {
            return disabled_route_result();
        }
        match ctx.sender() {
            Some(sender) => state.unregister_route(&payload.prefix, payload.method, sender),
            None => RegisterRouteResult::rejected(
                "aether.http.server.unregister_route_self requires a local sender; an \
                 external session or remote engine must use \
                 aether.http.server.unregister_route with an explicit handler path",
            ),
        }
    }

    /// Purge a departed mailbox's routes (ADR-0079 §8 amended). The
    /// substrate fires one notice per [`HttpSupervisorState::watch`]ed
    /// mailbox when it closes (the wasm trampoline on `DropComponent`),
    /// so the route table stops
    /// dispatching at a departed trampoline without any drop-time
    /// fan-out from the component host. Releasing the handle keeps the
    /// monitor map bounded by live route holders.
    ///
    /// The host stamps the departed holder as the notice's sender, so
    /// `ctx.sender()` is the same proven reference the monitor map and the
    /// route table's reverse index are keyed by (ADR-0230): both removals
    /// are keyed lookups, and only the holder's own routes are touched.
    #[handler::event]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        let Some(departed) = ctx.sender() else {
            return;
        };
        state.monitors.remove(&departed);
        state.unregister_routes_all(departed);
    }
}
