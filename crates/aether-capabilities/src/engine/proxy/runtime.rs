//! The `aether.engine.proxy:<id>` runtime half (ADR-0122 identity/runtime
//! split). Compiled only under `feature = "runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a transport-only build of
//! the [`EngineProxy`](super::EngineProxy) identity never names these types
//! nor pulls `aether_substrate`. The substrate-typed imports are gated once by
//! this module rather than line-by-line; the `#[actor] impl` reaches the
//! state, ctx types, and connect/heartbeat helpers through the single
//! `use runtime::*` glob in the parent.
//!
//! Native-only: the state owns a `TcpStream` (via [`RpcConnection`]) and an OS
//! thread (the heartbeat sidecar). `Drop` SIGKILLs the forked child and joins
//! the heartbeat thread, so the RAII teardown follows the fields onto the
//! state.

use super::{EngineProxy, EngineProxyConfig};
pub use crate::engine::kinds::{CallSettled, EngineAlive, EngineDied};
use crate::engine::kinds::{EngineHeartbeatTick, ForwardEnvelope};
use crate::rpc::RpcInboundReady;
pub use crate::rpc::{MailEnvelope, MailboxAddress, RpcConnection, RpcError, WireFrame};
pub use aether_actor::Addressable;
use aether_actor::runtime;
pub use aether_data::{EngineId, Kind, KindId, MailboxId, mailbox_id_from_name};
pub use aether_kinds::DeathReason;
use aether_kinds::TerminateEngine;
pub use aether_substrate::Mail;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::mailer::Mailer;
pub use aether_substrate::mail::{Source, SourceAddr};
pub use std::collections::HashMap;
pub use std::process::Child;
pub use std::sync::Arc;

use super::heartbeat::HeartbeatHandle;
use crate::engine::EngineServer;

// The init-only bring-up helpers live in the native-only `connect` /
// `heartbeat` submodules; re-export them here so the parent's `use runtime::*`
// glob reaches them alongside the rest of the runtime half.
pub use super::connect::connect_proxy;
pub use super::heartbeat::spawn_heartbeat;

/// Mailbox of the engines cap (`aether.engine`) — where a proxy
/// reports its own liveness transitions (`EngineAlive` / `EngineDied`,
/// issue 1339). A compile-time const derived from
/// `<EngineServer as Addressable>::NAMESPACE`, so no host round-trip; matches
/// the `RpcServerCapability`'s own route lookup.
// Well-known engines-cap route shared with `RpcServerCapability`'s own
// lookup; a ctx-less free helper, so there is no sibling `ctx.actor::<_>()`
// to resolve through.
#[allow(clippy::disallowed_methods)]
fn engine_cap_mailbox() -> MailboxId {
    mailbox_id_from_name(<EngineServer as Addressable>::NAMESPACE)
}

/// `aether.engine.proxy:<id>` runtime state (ADR-0122 split): one outbound
/// RPC connection to one substrate, plus the in-flight reply-correlation
/// table. The addressing identity is the distinct ZST
/// [`EngineProxy`](super::EngineProxy); the dispatcher holds this as the
/// proxy's state and routes envelopes through the macro-emitted `Dispatch`
/// impl. Living in this private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct EngineProxyState {
    pub(super) engine_id: EngineId,
    /// Cached so `on_inbound_ready` can push correlation-preserving
    /// reply mail — `NativeCtx` doesn't expose `mailer()`, only
    /// `NativeInitCtx` does.
    pub(super) mailer: Arc<Mailer>,
    /// The live outbound connection: `.client` writes `Call`s,
    /// `.inbound` carries reply frames, `.reader` joins on drop.
    /// `.server` holds the substrate's `HelloAck` identity (the
    /// kind manifest P4's describe handler will read).
    pub(super) conn: RpcConnection,
    /// wire `cid` → the `Source` of the `ForwardEnvelope` that
    /// opened the call. `ReplyEvent` frames route back here;
    /// `ReplyEnd` clears the entry.
    pub(super) in_flight: HashMap<u64, Source>,
    /// The forked child substrate, when the engines cap spawned it
    /// (see [`EngineProxyConfig::spawned`]). `Drop` SIGKILLs +
    /// reaps it; `None` once taken or for an adopted substrate.
    pub(super) spawned: Option<Child>,
    /// Consecutive heartbeat pings sent without a `Pong` reply
    /// (issue 1339). Incremented each `on_heartbeat_tick`, reset to
    /// `0` on any inbound `Pong`. Crossing `miss_limit` evicts the
    /// engine.
    pub(super) missed_heartbeats: u32,
    /// Consecutive-miss threshold that marks the engine dead. `0`
    /// when the heartbeat is disabled (`heartbeat: None`), in which
    /// case `on_heartbeat_tick` never fires anyway.
    pub(super) miss_limit: u32,
    /// Monotonic nonce stamped on each heartbeat `Ping` — for log
    /// correlation only; a `Pong` carrying any nonce counts as
    /// liveness, since there is at most one heartbeat outstanding.
    pub(super) heartbeat_seq: u64,
    /// The heartbeat timer thread, when armed. `Drop` stops + joins
    /// it. Held as the field's RAII guard — the leading `_` marks
    /// it as owned-for-its-Drop, not read.
    pub(super) _heartbeat: Option<HeartbeatHandle>,
}

impl Drop for EngineProxyState {
    /// SIGKILL + reap the child substrate this proxy forked, so a
    /// terminated proxy (or a chassis teardown) never orphans a
    /// substrate process. A no-op for an adopted substrate
    /// (`spawned` is `None`). Graceful SIGTERM is a follow-up;
    /// v1 is forceful.
    fn drop(&mut self) {
        if let Some(mut child) = self.spawned.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl EngineProxyState {
    /// Report a confirmed liveness signal to the engines cap so it
    /// refreshes this engine's last-heartbeat timestamp (issue
    /// 1339). Sent as a fresh root: the `Pong` that triggered it is
    /// an external event causally unrelated to whatever inbound
    /// mail woke the handler.
    pub(super) fn report_alive(&self, ctx: &NativeCtx<'_>) {
        let alive = EngineAlive {
            engine_id: self.engine_id.0.to_string(),
        };
        let _ = ctx.send_envelope_as_root(
            engine_cap_mailbox(),
            <EngineAlive as Kind>::ID,
            &alive.encode_into_bytes(),
        );
    }

    /// Report this engine's death to the engines cap so it drops the
    /// registry entry and records the cause in its recently-died ring
    /// (issue 1339, issue 1906). `reason` distinguishes a crash
    /// (`Crashed`, connection-close) from a heartbeat eviction
    /// (`Evicted`); a deliberate terminate never reaches here.
    /// Idempotent on the cap side — a `died` for an already-evicted
    /// engine is a no-op. Sent as a fresh root for the same reason as
    /// [`Self::report_alive`].
    pub(super) fn report_died(&self, ctx: &NativeCtx<'_>, reason: DeathReason) {
        let died = EngineDied {
            engine_id: self.engine_id.0.to_string(),
            reason,
        };
        let _ = ctx.send_envelope_as_root(
            engine_cap_mailbox(),
            <EngineDied as Kind>::ID,
            &died.encode_into_bytes(),
        );
    }

    /// Route a `ReplyEvent`'s envelope back to whoever sent the
    /// `ForwardEnvelope` that opened `cid`. Mirrors
    /// `Mailer::send_reply`'s `Component` branch: push a `Mail`
    /// carrying the reply kind + already-encoded bytes, with the
    /// original `correlation_id` echoed (reply-to `None` — nobody
    /// replies to a reply) so a correlation-matching caller picks
    /// it up.
    pub(super) fn route_reply(&mut self, cid: u64, envelope: MailEnvelope) {
        let Some(reply_to) = self.in_flight.get(&cid).copied() else {
            tracing::debug!(
                target: "aether_substrate::engine_proxy",
                engine_id = ?self.engine_id,
                cid,
                "engine proxy: ReplyEvent with no matching in-flight forward; dropping",
            );
            return;
        };
        let SourceAddr::Component(target) = reply_to.addr else {
            // The `ForwardEnvelope` arrived with no `Component`
            // reply target (broadcast / `None`) — there's nowhere
            // local to route the reply.
            return;
        };
        self.mailer.push(
            Mail::new(target, envelope.kind, envelope.payload, 1).with_reply_to(
                Source::with_correlation(SourceAddr::None, reply_to.correlation_id),
            ),
        );
    }

    /// Lift the substrate's terminal `ReplyEnd` for `cid` into a
    /// [`CallSettled`] mail back to whoever opened the call, then
    /// clear the in-flight entry. Mirrors [`Self::route_reply`]'s
    /// correlation handling — a forwarded call has no local chain
    /// to settle, so this explicit terminal signal is how the
    /// originating `RpcServerCapability` learns to close its wire
    /// call. The wire `RpcError` is rendered to a string; the
    /// `aether-kinds` layer can't carry the structured variant.
    pub(super) fn route_settled(&mut self, cid: u64, result: Result<(), RpcError>) {
        let Some(reply_to) = self.in_flight.remove(&cid) else {
            tracing::debug!(
                target: "aether_substrate::engine_proxy",
                engine_id = ?self.engine_id,
                cid,
                "engine proxy: ReplyEnd with no matching in-flight forward; dropping",
            );
            return;
        };
        let SourceAddr::Component(target) = reply_to.addr else {
            return;
        };
        let settled = match result {
            Ok(()) => CallSettled::Ok,
            Err(e) => CallSettled::Err {
                error: format!("{e:?}"),
            },
        };
        self.mailer.push(
            Mail::new(
                target,
                <CallSettled as Kind>::ID,
                settled.encode_into_bytes(),
                1,
            )
            .with_reply_to(Source::with_correlation(
                SourceAddr::None,
                reply_to.correlation_id,
            )),
        );
    }
}

#[runtime]
impl NativeActor for EngineProxy {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// per-engine outbound RPC connection plus the in-flight
    /// reply-correlation table.
    type State = EngineProxyState;
    type Config = EngineProxyConfig;
    const NAMESPACE: &'static str = "aether.engine.proxy";

    fn init(
        mut config: EngineProxyConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<EngineProxyState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let wake_kind = KindId(<RpcInboundReady as Kind>::ID.0);

        // A freshly-forked substrate (`spawned.is_some()`) may not
        // have bound its RPC port yet, so the startup dial retries
        // briefly. An adopted / externally-running substrate
        // (`spawned.is_none()`) is dialed once — a refused
        // connection there is a real error, not a startup race.
        let retry = config.spawned.is_some();
        let conn = match connect_proxy(
            &config.rpc_addr,
            &mailer,
            self_mailbox,
            wake_kind,
            retry,
            config.connect_budget,
            config.spawned.as_mut(),
        ) {
            Ok(conn) => conn,
            Err(e) => {
                // The proxy owns the child it was handed — a
                // failed boot must not orphan the substrate.
                if let Some(mut child) = config.spawned.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(BootError::Other(Box::new(e)));
            }
        };

        tracing::info!(
            target: "aether_substrate::engine_proxy",
            engine_id = ?config.engine_id,
            addr = %config.rpc_addr,
            spawned = config.spawned.is_some(),
            "engine proxy connected",
        );

        // Arm the liveness heartbeat, if configured. The sidecar
        // thread fires an `EngineHeartbeatTick` at this proxy's own
        // mailbox every `interval`; `on_heartbeat_tick` does the
        // ping + miss accounting on the dispatcher thread (so the
        // RPC write and all proxy state stay single-threaded).
        let (heartbeat, miss_limit) = match config.heartbeat {
            Some(params) if !params.interval.is_zero() && params.miss_limit > 0 => {
                let handle = spawn_heartbeat(Arc::clone(&mailer), self_mailbox, params.interval);
                (Some(handle), params.miss_limit)
            }
            _ => (None, 0),
        };

        Ok(EngineProxyState {
            engine_id: config.engine_id,
            mailer,
            conn,
            in_flight: HashMap::new(),
            spawned: config.spawned,
            missed_heartbeats: 0,
            miss_limit,
            heartbeat_seq: 0,
            _heartbeat: heartbeat,
        })
    }

    /// Relay one mail to the substrate as an RPC `Call`.
    ///
    /// # Agent
    /// Hand the proxy a `ForwardEnvelope { mailbox, kind, payload }`
    /// — the `mailbox` is the *substrate-local* recipient, `kind` +
    /// `payload` the mail to deliver there. Any reply routes back to
    /// the sender of this `ForwardEnvelope`.
    #[handler]
    fn on_forward(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: ForwardEnvelope) {
        let envelope = MailEnvelope {
            to: MailboxAddress::local(mail.mailbox),
            from: None,
            kind: mail.kind,
            correlation_id: None,
            payload: mail.payload,
        };
        match state.conn.client.call(envelope) {
            Ok(cid) => {
                state.in_flight.insert(cid, ctx.reply_target());
            }
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::engine_proxy",
                    engine_id = ?state.engine_id,
                    error = %e,
                    "engine proxy: Call write failed; dropping forward",
                );
            }
        }
    }

    /// Reader-sidecar wake. Drain every inbound frame.
    ///
    /// # Agent
    /// Internal wake mail — not part of the proxy's external
    /// surface. The reader thread fires this after pushing a frame;
    /// the handler drains `conn.inbound` and routes each frame.
    #[handler]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: RpcInboundReady) {
        while let Ok(frame) = state.conn.inbound.try_recv() {
            match frame {
                WireFrame::ReplyEvent { cid, envelope } => state.route_reply(cid, envelope),
                WireFrame::ReplyEnd { cid, result } => state.route_settled(cid, result),
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
                        target: "aether_substrate::engine_proxy",
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
                        target: "aether_substrate::engine_proxy",
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
    /// Sent by the engines cap (`aether.engine`) on a terminate
    /// request. The proxy self-shuts-down; its `Drop` SIGKILLs and
    /// reaps the child substrate it forked (if any), and the
    /// outbound RPC connection closes as the actor drops. The
    /// `engine_id` field is ignored — a proxy only ever terminates
    /// its own engine.
    #[handler]
    fn on_terminate(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: TerminateEngine) {
        tracing::info!(
            target: "aether_substrate::engine_proxy",
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
    /// to the engines cap and self-shuts-down (its `Drop` SIGKILLs
    /// the wedged child). Otherwise it sends a fresh `Ping`.
    #[handler]
    fn on_heartbeat_tick(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _mail: EngineHeartbeatTick,
    ) {
        state.heartbeat_seq += 1;
        // A write failure means the socket is already broken — the
        // reader sidecar will surface a `Bye` and `on_inbound_ready`
        // handles the eviction. Count it as a miss and carry on so
        // the miss-limit path also covers it (whichever fires first
        // evicts; the cap side is idempotent).
        if let Err(e) = state.conn.client.ping(state.heartbeat_seq) {
            tracing::debug!(
                target: "aether_substrate::engine_proxy",
                engine_id = ?state.engine_id,
                error = %e,
                "engine proxy: heartbeat ping write failed",
            );
        }
        state.missed_heartbeats += 1;
        if state.missed_heartbeats >= state.miss_limit {
            tracing::warn!(
                target: "aether_substrate::engine_proxy",
                engine_id = ?state.engine_id,
                missed = state.missed_heartbeats,
                miss_limit = state.miss_limit,
                "engine proxy: heartbeat miss limit crossed; evicting engine",
            );
            state.report_died(
                ctx,
                DeathReason::Evicted {
                    detail: format!(
                        "heartbeat miss limit {} of {}",
                        state.missed_heartbeats, state.miss_limit
                    ),
                },
            );
            ctx.shutdown();
        }
    }
}
