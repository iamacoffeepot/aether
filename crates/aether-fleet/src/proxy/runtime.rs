//! The `aether.fleet.proxy:<id>` runtime half (ADR-0122 identity/runtime
//! split). The [`FleetProxy`] identity file names none of
//! these types. The substrate-typed imports are collected once by
//! this module rather than line-by-line; the `#[actor] impl` reaches the
//! state, ctx types, and connect/heartbeat helpers through the single
//! `use runtime::*` glob in the parent.
//!
//! Native-only: the state owns a `TcpStream` (via [`RpcConnection`]) and an OS
//! thread (the heartbeat sidecar). `Drop` terminates the forked child's process
//! group and joins the heartbeat thread, so the RAII teardown follows the
//! fields onto the state.

use super::config::ProxyTarget;
use super::{FleetProxy, FleetProxyConfig};
use crate::kinds::EngineHeartbeatTick;
pub use crate::kinds::{EngineAlive, EngineDied};
use aether_actor::{Manual, OutboundReply, Single, runtime};
pub use aether_data::EngineId;
pub use aether_kinds::DeathReason;
use aether_kinds::TerminateEngine;
pub use aether_rpc::{CallSettled, MailEnvelope, Recipient, ReplyEnvelope, RpcConnection, RpcError, WireFrame};
use aether_rpc::{
    ForwardEnvelope, RegisterEngineRoute, RegisterEngineRouteResult, RpcInboundReady, RpcServerCapability,
};
pub use aether_substrate::actor::native::{DeferredReply, NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::runtime::trace::SettlementHold;
pub use std::collections::HashMap;
use std::mem;
pub use std::process::Child;

use super::heartbeat::HeartbeatHandle;
use super::reap::terminate_child_group;
use crate::FleetServer;

// The init-only bring-up helpers live in the native-only `connect` /
// `heartbeat` submodules; re-export them here so the parent's `use runtime::*`
// glob reaches them alongside the rest of the runtime half.
pub use super::connect::connect_proxy;
pub use super::heartbeat::spawn_heartbeat;

/// `aether.fleet.proxy:<id>` runtime state (ADR-0122 split): one outbound
/// RPC connection to one substrate, plus the in-flight reply-correlation
/// table. The addressing identity is the distinct ZST
/// [`FleetProxy`]; the dispatcher holds this as the
/// proxy's state and routes envelopes through the macro-emitted `Dispatch`
/// impl. Living in this private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct FleetProxyState {
    pub engine_id: EngineId,
    /// The live outbound connection: `.client` writes `Call`s,
    /// `.inbound` carries reply frames, `.reader` joins on drop.
    /// `.server` holds the substrate's `HelloAck` identity (the
    /// kind manifest P4's describe handler will read).
    pub conn: RpcConnection,
    /// wire `cid` → the reply owed to whoever sent the
    /// `ForwardEnvelope` that opened the call. Each entry is the debt for
    /// one open forward: `ReplyEvent` frames relay through it, and it
    /// leaves only when its `ReplyEnd` answers it or, when this actor
    /// closes, through `unwire`'s abandonment. It is never evicted.
    pub in_flight: HashMap<u64, DeferredReply>,
    /// The forked child substrate, when the engines cap spawned it
    /// (see [`ProxyTarget::Forked`]). `Drop` terminates its
    /// process group + reaps it; `None` once taken or for an adopted
    /// substrate.
    pub spawned: Option<Child>,
    /// Consecutive heartbeat pings sent without a `Pong` reply
    /// (issue 1339). Incremented each `on_heartbeat_tick`, reset to
    /// `0` on any inbound `Pong`. Crossing `miss_limit` evicts the
    /// engine.
    pub missed_heartbeats: u32,
    /// Consecutive-miss threshold that marks the engine dead. `0`
    /// when the heartbeat is disabled (`heartbeat: None`), in which
    /// case `on_heartbeat_tick` never fires anyway.
    pub miss_limit: u32,
    /// Monotonic nonce stamped on each heartbeat `Ping` — for log
    /// correlation only; a `Pong` carrying any nonce counts as
    /// liveness, since there is at most one heartbeat outstanding.
    pub heartbeat_seq: u64,
    /// The heartbeat timer thread, when armed. `Drop` stops + joins
    /// it. Held as the field's RAII guard — the leading `_` marks
    /// it as owned-for-its-Drop, not read.
    _heartbeat: Option<HeartbeatHandle>,
    /// Holds the chain that spawned this proxy open until the hub's RPC
    /// server answers its `RegisterEngineRoute`, so a `SpawnEngine` settles
    /// only once the new engine is routable. `None` before `wire`, once
    /// the answer arrives, and for a proxy no chain caused (adopted or
    /// test-spawned).
    pub route_hold: Option<SettlementHold>,
}

impl Drop for FleetProxyState {
    /// Terminate + reap the child substrate this proxy forked, so a
    /// terminated proxy (or a chassis teardown) never orphans a
    /// substrate process — nor anything the substrate forked, which a
    /// bare kill of the recorded pid would leave behind. The escalation
    /// itself (SIGTERM the group, grace, SIGKILL) is
    /// [`terminate_child_group`]. A no-op for an adopted substrate
    /// (`spawned` is `None`).
    fn drop(&mut self) {
        if let Some(mut child) = self.spawned.take() {
            terminate_child_group(&mut child);
        }
    }
}

impl FleetProxyState {
    /// Report a confirmed liveness signal to the engines cap so it
    /// refreshes this engine's last-heartbeat timestamp (issue
    /// 1339). Sent to the declared dependency as a fresh root: the `Pong`
    /// that triggered it is an external event causally unrelated to
    /// whatever inbound mail woke the handler.
    pub fn report_alive(&self, ctx: &mut NativeCtx<'_, FleetProxy, Single>) {
        ctx.send_detached::<FleetServer>(&EngineAlive { engine_id: self.engine_id.0.to_string() });
    }

    /// Report this engine's death to the engines cap so it drops the
    /// registry entry and records the cause in its recently-died ring
    /// (issue 1339, issue 1906). `reason` distinguishes a crash
    /// (`Crashed`, connection-close) from a heartbeat eviction
    /// (`Evicted`); a deliberate terminate never reaches here.
    /// Idempotent on the cap side — a `died` for an already-evicted
    /// engine is a no-op. Sent to the declared dependency as a fresh root,
    /// for the same reason as [`Self::report_alive`].
    pub fn report_died(&self, ctx: &mut NativeCtx<'_, FleetProxy, Single>, reason: DeathReason) {
        ctx.send_detached::<FleetServer>(&EngineDied { engine_id: self.engine_id.0.to_string(), reason });
    }

    /// Relay a `ReplyEvent`'s envelope to whoever sent the
    /// `ForwardEnvelope` that opened `cid`, through the debt parked for it:
    /// the already-encoded reply goes out under the root the debt keeps
    /// open, with the caller's correlation echoed, and the debt stays owed
    /// until the call's `ReplyEnd`. An event for a `cid` with no open
    /// forward (one that arrives after its terminal) is dropped.
    pub fn route_reply(&mut self, ctx: &mut NativeCtx<'_, FleetProxy, Single>, cid: u64, envelope: &ReplyEnvelope) {
        let Some(owed) = self.in_flight.get(&cid) else {
            tracing::debug!(
                target: "aether_substrate::fleet_proxy",
                engine_id = ?self.engine_id,
                cid,
                "engine proxy: ReplyEvent with no matching in-flight forward; dropping",
            );
            return;
        };
        owed.reply_envelope(ctx, envelope.kind, &envelope.payload);
    }

    /// Lift the substrate's terminal `ReplyEnd` for `cid` into a
    /// [`CallSettled`] reply that discharges the debt parked for it. A
    /// forwarded call has no local chain to settle, so this explicit
    /// terminal signal is how the originating `RpcServerCapability`
    /// learns to close its wire call. The wire `RpcError` rides in
    /// `CallSettled::Err` as it arrived, so the hub writes the
    /// substrate's refusal (a `NotPresent` naming the path, say) to its
    /// caller unchanged.
    pub fn route_settled(
        &mut self,
        ctx: &mut NativeCtx<'_, FleetProxy, Single>,
        cid: u64,
        result: Result<(), RpcError>,
    ) {
        let Some(owed) = self.in_flight.remove(&cid) else {
            tracing::debug!(
                target: "aether_substrate::fleet_proxy",
                engine_id = ?self.engine_id,
                cid,
                "engine proxy: ReplyEnd with no matching in-flight forward; dropping",
            );
            return;
        };
        let settled = match result {
            Ok(()) => CallSettled::Ok,
            Err(error) => CallSettled::Err { error },
        };
        owed.reply(ctx, &settled);
    }
}

#[runtime]
impl NativeActor for FleetProxy {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// per-engine outbound RPC connection plus the in-flight
    /// reply-correlation table.
    type State = FleetProxyState;
    type Config = FleetProxyConfig;
    const NAMESPACE: &'static str = "aether.fleet.proxy";

    fn init(mut config: FleetProxyConfig, ctx: &mut NativeInitCtx<'_>) -> Result<FleetProxyState, BootError> {
        let wake = ctx.self_wake::<RpcInboundReady>();

        // Take the target out of the config, leaving a childless husk, so
        // the child moves into this proxy's state and the config's `Drop`
        // has nothing left to terminate. A forked substrate is dialed only
        // on the port it reports, while it lives; an adopted one once.
        let mut target = mem::replace(&mut config.target, ProxyTarget::Adopted { rpc_addr: String::new() });
        let (conn, addr) = match connect_proxy(&mut target, &wake, config.connect_budget) {
            Ok(connected) => connected,
            Err(e) => {
                // The proxy owns the child it was handed — a failed
                // boot must not orphan the substrate, and the same
                // group escalation `Drop` runs is what makes that
                // true for whatever the substrate itself forked.
                if let ProxyTarget::Forked { mut child, .. } = target {
                    terminate_child_group(&mut child);
                }
                return Err(BootError::Other(Box::new(e)));
            }
        };
        let spawned = match target {
            ProxyTarget::Forked { child, .. } => Some(child),
            ProxyTarget::Adopted { .. } => None,
        };

        tracing::info!(
            target: "aether_substrate::fleet_proxy",
            engine_id = ?config.engine_id,
            addr = %addr,
            spawned = spawned.is_some(),
            "engine proxy connected",
        );

        // Arm the liveness heartbeat, if configured. The sidecar
        // thread wakes this proxy with an `EngineHeartbeatTick` every
        // `interval`; `on_heartbeat_tick` does the
        // ping + miss accounting on the dispatcher thread (so the
        // RPC write and all proxy state stay single-threaded).
        let (heartbeat, miss_limit) = match config.heartbeat {
            Some(params) if !params.interval.is_zero() && params.miss_limit > 0 => {
                let handle = spawn_heartbeat(ctx.self_wake::<EngineHeartbeatTick>(), params.interval);
                (Some(handle), params.miss_limit)
            }
            _ => (None, 0),
        };

        Ok(FleetProxyState {
            engine_id: config.engine_id,
            conn,
            in_flight: HashMap::new(),
            spawned,
            missed_heartbeats: 0,
            miss_limit,
            heartbeat_seq: 0,
            _heartbeat: heartbeat,
            route_hold: None,
        })
    }

    /// Abandon every forward still open, since this actor is closing and
    /// can no longer answer it. The hub's RPC server monitors this proxy
    /// and closes each wire call still in flight here when it departs.
    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        for (_, owed) in state.in_flight.drain() {
            owed.abandon_for_actor_close();
        }
    }

    /// Register this proxy as its engine's route, then fire one
    /// post-registration catch-up wake.
    ///
    /// The hub's RPC server forwards every `engine = Some(engine_id)` wire
    /// `Call` to the proxy that registered for `engine_id`. A send from
    /// `wire` starts a fresh root, so the registration alone would not keep
    /// the spawn open; the settlement hold on the causing chain (ADR-0168)
    /// does, until [`Self::on_route_registered`] drops it. The proxy declares
    /// the RPC server as a dependency, so the registration always has a live
    /// recipient.
    ///
    /// The reader starts during `init`, before an instanced proxy's mailbox
    /// is published, so an early frame can enqueue successfully while its
    /// accompanying wake is dropped as unresolved. `wire` runs after
    /// publication; the self-wake ensures the dispatcher drains any frame
    /// stranded in that gap.
    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        state.route_hold = ctx.acquire_settlement_hold();
        ctx.send::<RpcServerCapability>(&RegisterEngineRoute { engine_id: state.engine_id });
        ctx.self_wake::<RpcInboundReady>().wake(&RpcInboundReady::default());
    }

    /// The hub RPC server's answer to this proxy's route registration.
    ///
    /// # Agent
    /// Internal — the reply to the `RegisterEngineRoute` this proxy sent
    /// from `wire`. Releases the spawn chain's hold; an `Err` means
    /// engine-addressed calls will not reach this proxy, and is logged.
    #[handler::single]
    fn on_route_registered(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: RegisterEngineRouteResult) {
        state.route_hold = None;
        if let RegisterEngineRouteResult::Err { error } = mail {
            tracing::warn!(
                target: "aether_substrate::fleet_proxy",
                engine_id = ?state.engine_id,
                error = %error,
                "engine proxy: route registration refused; engine-addressed calls will not reach this engine",
            );
        }
    }

    /// Relay one mail to the substrate as an RPC `Call`.
    ///
    /// # Agent
    /// Hand the proxy a `ForwardEnvelope { recipient, kind, payload }`
    /// — `recipient` is the substrate-local actor's `ActorPath`, sent on
    /// as written for the substrate to resolve, and `kind` + `payload`
    /// the mail to deliver there. Every reply the substrate streams back
    /// relays to the sender of this `ForwardEnvelope`, and a
    /// `CallSettled` ends the exchange, a `CallSettled::Err` naming the
    /// failure when the call cannot be written to the engine.
    #[handler::manual]
    fn on_forward(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, mail: ForwardEnvelope) {
        let envelope = MailEnvelope { to: Recipient::local(mail.recipient), kind: mail.kind, payload: mail.payload };
        match state.conn.client.call(envelope) {
            Ok(cid) => {
                state.in_flight.insert(cid, ctx.defer_reply_to(ctx.reply_target()));
            }
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::fleet_proxy",
                    engine_id = ?state.engine_id,
                    error = %e,
                    "engine proxy: Call write failed; answering the forward with the error",
                );
                ctx.reply(&CallSettled::Err {
                    error: RpcError::Other {
                        reason: format!("engine proxy could not write the call to its engine: {e}"),
                    },
                });
            }
        }
    }

    /// Reader-sidecar wake. Drain every inbound frame.
    ///
    /// # Agent
    /// Internal wake mail — not part of the proxy's external
    /// surface. The reader thread fires this after pushing a frame;
    /// the handler drains `conn.inbound` and routes each frame.
    #[handler::single]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Single>, _mail: RpcInboundReady) {
        while let Ok(frame) = state.conn.inbound.try_recv() {
            match frame {
                WireFrame::ReplyEvent { cid, envelope } => state.route_reply(ctx, cid, &envelope),
                WireFrame::ReplyEnd { cid, result } => state.route_settled(ctx, cid, result),
                // A `Pong` answers this proxy's heartbeat `Ping`
                // (issue 1339): the substrate is alive. Clear the
                // miss counter and report the liveness up to the
                // engines cap so `list_engines` can show a fresh
                // heartbeat age. The nonce is for log correlation
                // only — any `Pong` is a liveness signal.
                WireFrame::Pong(_nonce) => {
                    state.missed_heartbeats = 0;
                    state.report_alive(ctx);
                }
                WireFrame::Bye { reason } => {
                    tracing::info!(
                        target: "aether_substrate::fleet_proxy",
                        engine_id = ?state.engine_id,
                        reason = %reason,
                        "engine proxy: substrate closed the connection; shutting down",
                    );
                    // Tell the engines cap the engine is gone so it
                    // drops the registry entry — without this the
                    // proxy dies but `list_engines` keeps reporting
                    // a corpse (issue 1339). The substrate closed the
                    // connection on its own — a crash, not a
                    // deliberate terminate; carry the `Bye` reason so
                    // `list_engines` can show why.
                    state.report_died(ctx, DeathReason::Crashed { detail: reason });
                    ctx.shutdown();
                    return;
                }
                // Hello / HelloAck / Call / Ping: a client-side proxy
                // never expects these inbound. Drop with a debug
                // line rather than warn-storming.
                other => {
                    tracing::debug!(
                        target: "aether_substrate::fleet_proxy",
                        engine_id = ?state.engine_id,
                        frame = ?other,
                        "engine proxy: unexpected inbound frame; ignoring",
                    );
                }
            }
        }
    }

    /// Shut this proxy's substrate down.
    ///
    /// # Agent
    /// Sent by the engines cap (`aether.fleet`) on a terminate
    /// request. The proxy self-shuts-down; its `Drop` terminates and
    /// reaps the process group of the child substrate it forked (if
    /// any), and the
    /// outbound RPC connection closes as the actor drops. The
    /// `engine_id` field is ignored — a proxy only ever terminates
    /// its own engine.
    #[handler::single]
    fn on_terminate(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: TerminateEngine) {
        tracing::info!(
            target: "aether_substrate::fleet_proxy",
            engine_id = ?state.engine_id,
            "engine proxy: terminate requested; shutting down",
        );
        // No `report_died` here: the engines cap initiated this
        // terminate and already dropped the registry entry, so the
        // proxy reporting back would be a redundant (idempotent)
        // no-op. The self-death paths (`Bye`, heartbeat timeout) are
        // the ones the cap doesn't already know about.
        ctx.shutdown();
    }

    /// Liveness-heartbeat timer wake (issue 1339).
    ///
    /// # Agent
    /// Internal wake mail — not part of the proxy's external
    /// surface. The heartbeat sidecar thread fires this every
    /// interval. The handler counts the tick as an outstanding miss
    /// (a `Pong` since the last tick would have cleared the
    /// counter), and once `miss_limit` consecutive ticks go
    /// unanswered it declares the engine dead: reports `EngineDied`
    /// to the engines cap and self-shuts-down (its `Drop` terminates
    /// the wedged child's group). Otherwise it sends a fresh `Ping`.
    #[handler::single]
    fn on_heartbeat_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Single>, _mail: EngineHeartbeatTick) {
        state.heartbeat_seq += 1;
        // A write failure means the socket is already broken — the
        // reader sidecar will surface a `Bye` and `on_inbound_ready`
        // handles the eviction. Count it as a miss and carry on so
        // the miss-limit path also covers it (whichever fires first
        // evicts; the cap side is idempotent).
        if let Err(e) = state.conn.client.ping(state.heartbeat_seq) {
            tracing::debug!(
                target: "aether_substrate::fleet_proxy",
                engine_id = ?state.engine_id,
                error = %e,
                "engine proxy: heartbeat ping write failed",
            );
        }
        state.missed_heartbeats += 1;
        if state.missed_heartbeats >= state.miss_limit {
            tracing::warn!(
                target: "aether_substrate::fleet_proxy",
                engine_id = ?state.engine_id,
                missed = state.missed_heartbeats,
                miss_limit = state.miss_limit,
                "engine proxy: heartbeat miss limit crossed; evicting engine",
            );
            state.report_died(
                ctx,
                DeathReason::Evicted {
                    detail: format!("heartbeat miss limit {} of {}", state.missed_heartbeats, state.miss_limit),
                },
            );
            ctx.shutdown();
        }
    }
}
