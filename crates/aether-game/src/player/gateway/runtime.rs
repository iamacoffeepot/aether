//! Runtime half of the player gateway.

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;

use aether_actor::runtime;
use aether_data::{Kind, MailboxId};
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnApplied, SpawnError, TaskDone};
use aether_substrate::chassis::error::BootError;

use super::{BindListenerResult, GameGatewayCapability, MonitorNotice, SessionClosed, SessionData, TickBundle};
use crate::player::PlayerSessionActor;
use crate::player::session::PlayerSessionConfig;
use aether_tcp::{TcpCapability, TcpNativeExt, TcpSessionActor};

const DEFAULT_LISTENER_NAME: &str = "players";
const DEFAULT_INTERVAL_NANOS: u64 = 1_000_000_000 / 60;

/// Inert-by-default game-listener config (ADR-0156 §3). The operator-typable
/// listener + session knobs; the resolved authoritative-simulation mailbox is
/// composer wiring and rides [`GameGatewayParams`] instead.
///
/// An active server supplies both `listener_addr` (here) and `turn_sim_mailbox`
/// (on `GameGatewayParams`). The gateway captures its resolved `ctx.self_id()`
/// at init, then actor wiring binds `listener_addr` under `listener_name` and
/// passes that exact mailbox as the tcp consumer.
/// `listener_name` is retained only to address the trusted tcp listener/session
/// topology for outbound writes; it never enters the player wire as a recipient.
///
/// ADR-0090: the `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `GameGatewayConfigLayer`, the clap-shaped `GameGatewayOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` / `from_argv_then_env`
/// shims. `env_prefix = "AETHER_GAME_GATEWAY"` joins the field env keys.
#[derive(Clone, Debug, PartialEq, Eq, aether_substrate::Config)]
#[config(env_prefix = "AETHER_GAME_GATEWAY", cli_prefix = "game-gateway")]
pub struct GameGatewayConfig {
    /// Address the player gateway listens on; unset leaves the gateway inert.
    pub listener_addr: Option<String>,
    /// Name of the trusted TCP listener topology used for transport demultiplexing.
    #[config(default = "players")]
    pub listener_name: String,
    /// Authoritative simulation interval in nanoseconds sent in player clock beacons.
    ///
    /// Default `1_000_000_000 / 60` (60 Hz).
    #[config(default = 16_666_666)]
    pub interval_nanos: u64,
    /// Maximum number of simultaneously supervised player sessions.
    #[config(default = 1024)]
    pub max_active_sessions: usize,
    /// Maximum distinct live ticks buffered by each catching-up player session.
    #[config(default = 64)]
    pub max_pending_live_bundles: usize,
}

impl GameGatewayConfig {
    /// Default active-session ceiling for a configured common chassis.
    pub const DEFAULT_MAX_ACTIVE_SESSIONS: usize = 1_024;
    /// Default per-session live-fact ceiling while catch-up is in flight.
    pub const DEFAULT_MAX_PENDING_LIVE_BUNDLES: usize = 64;
}

impl Default for GameGatewayConfig {
    fn default() -> Self {
        Self {
            listener_addr: None,
            listener_name: DEFAULT_LISTENER_NAME.into(),
            interval_nanos: DEFAULT_INTERVAL_NANOS,
            max_active_sessions: Self::DEFAULT_MAX_ACTIVE_SESSIONS,
            max_pending_live_bundles: Self::DEFAULT_MAX_PENDING_LIVE_BUNDLES,
        }
    }
}

/// Composer-supplied construction params for `GameGatewayCapability`
/// (ADR-0156 §3). The exact resolved `TurnSim` mailbox used for intent
/// dispatch and polling — a resolved `MailboxId`, so by definition `Params`,
/// never `Config`. `None` leaves the gateway inert.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameGatewayParams {
    /// Exact resolved `TurnSim` mailbox used for intent dispatch and polling.
    pub turn_sim_mailbox: Option<MailboxId>,
}

pub struct GameGatewayState {
    self_mailbox: MailboxId,
    listener_addr: Option<String>,
    listener_name: String,
    turn_sim_mailbox: Option<MailboxId>,
    interval_nanos: u64,
    max_active_sessions: usize,
    max_pending_live_bundles: usize,
    pending_sessions: HashMap<String, PendingPlayerSession>,
    sessions: HashMap<String, PlayerSessionEntry>,
    session_by_child: HashMap<MailboxId, String>,
}

struct PendingPlayerSession {
    child: MailboxId,
    cancellation: Option<SessionClosed>,
}

#[derive(Clone)]
struct PlayerSessionContinuation {
    session_name: String,
    peer: String,
}

struct PlayerSessionEntry {
    child: MailboxId,
    _monitor: MonitorHandle,
}

#[runtime]
impl NativeActor for GameGatewayCapability {
    type State = GameGatewayState;
    type Config = GameGatewayConfig;
    type Params = GameGatewayParams;
    const NAMESPACE: &'static str = "aether.game.gateway";

    fn init(
        config: GameGatewayConfig,
        params: GameGatewayParams,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<GameGatewayState, BootError> {
        Ok(GameGatewayState {
            self_mailbox: ctx.self_id(),
            listener_addr: config.listener_addr,
            listener_name: config.listener_name,
            turn_sim_mailbox: params.turn_sim_mailbox,
            interval_nanos: config.interval_nanos,
            max_active_sessions: config.max_active_sessions,
            max_pending_live_bundles: config.max_pending_live_bundles,
            pending_sessions: HashMap::new(),
            sessions: HashMap::new(),
            session_by_child: HashMap::new(),
        })
    }

    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        let (Some(listener_addr), Some(_)) = (state.listener_addr.as_deref(), state.turn_sim_mailbox) else {
            return;
        };

        ctx.actor::<TcpCapability>().bind_listener(listener_addr, Some(&state.listener_name), Some(state.self_mailbox));
    }

    fn unwire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        for (session_name, pending) in state.pending_sessions.drain() {
            let close_notice = pending.cancellation.unwrap_or_else(|| SessionClosed {
                session_name: session_name.clone(),
                peer: String::new(),
                reason: "game gateway shutting down".to_owned(),
            });
            let _ = ctx.send_envelope_tracked(pending.child, SessionClosed::ID, &close_notice.encode_into_bytes());
            ctx.actor::<TcpCapability>().session_close(&state.listener_name, &session_name);
        }
        for (session_name, entry) in state.sessions.drain() {
            let close_notice = SessionClosed {
                session_name: session_name.clone(),
                peer: String::new(),
                reason: "game gateway shutting down".to_owned(),
            };
            let _ = ctx.send_envelope_tracked(entry.child, SessionClosed::ID, &close_notice.encode_into_bytes());
            ctx.actor::<TcpCapability>().session_close(&state.listener_name, &session_name);
        }
        state.session_by_child.clear();
    }

    #[handler::single]
    fn on_bind_listener_result(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, result: BindListenerResult) {
        match result {
            BindListenerResult::Ok { listener_name, listener_id, local_port }
                if listener_name == state.listener_name =>
            {
                tracing::info!(
                    target: "aether_game",
                    listener = %listener_name,
                    listener_mailbox = %listener_id,
                    local_port,
                    "game gateway listener bound",
                );
            }
            BindListenerResult::Ok { listener_name, .. } => {
                tracing::warn!(
                    target: "aether_game",
                    expected = %state.listener_name,
                    actual = %listener_name,
                    "game gateway ignored a bind result for another listener",
                );
            }
            BindListenerResult::Err { addr, reason } => {
                tracing::warn!(
                    target: "aether_game",
                    %addr,
                    %reason,
                    "game gateway listener bind failed",
                );
            }
        }
    }

    #[handler::single]
    fn on_session_data(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SessionData) {
        if !state.is_trusted_tcp_session(ctx, &mail.session_name) {
            tracing::warn!(
                target: "aether_game",
                session = %mail.session_name,
                "game gateway dropped session data outside its configured tcp lineage",
            );
            return;
        }

        if let Some(entry) = state.sessions.get(&mail.session_name) {
            let _ = ctx.send_envelope_tracked(entry.child, SessionData::ID, &mail.encode_into_bytes());
            return;
        }

        if let Some(entry) = state.pending_sessions.get(&mail.session_name) {
            let _ = ctx.send_envelope_tracked(entry.child, SessionData::ID, &mail.encode_into_bytes());
            return;
        }

        if state.is_at_capacity() {
            tracing::warn!(
                target: "aether_game",
                session = %mail.session_name,
                max_active_sessions = state.max_active_sessions,
                "game gateway refused a tcp session at capacity",
            );
            ctx.actor::<TcpCapability>().session_close(&state.listener_name, &mail.session_name);
            return;
        }

        let Some(turn_sim_mailbox) = state.turn_sim_mailbox else {
            ctx.actor::<TcpCapability>().session_close(&state.listener_name, &mail.session_name);
            return;
        };

        let session_name = mail.session_name.clone();
        let continuation = PlayerSessionContinuation { session_name: session_name.clone(), peer: mail.peer.clone() };
        let receipt = match ctx
            .spawn_child::<GameGatewayCapability, PlayerSessionActor>(
                Subname::Named(&session_name),
                PlayerSessionConfig {
                    listener_name: state.listener_name.clone(),
                    session_name: session_name.clone(),
                    peer: mail.peer.clone(),
                    turn_sim_mailbox,
                    interval_nanos: state.interval_nanos,
                    max_pending_live_bundles: state.max_pending_live_bundles,
                },
                (),
            )
            .after_init(mail)
            .stage_with(continuation)
        {
            Ok(receipt) => receipt,
            Err(error) => {
                tracing::warn!(
                    target: "aether_game",
                    session = %session_name,
                    error = ?error,
                    "player session spawn failed",
                );
                ctx.actor::<TcpCapability>().session_close(&state.listener_name, &session_name);
                return;
            }
        };

        let replaced = state
            .pending_sessions
            .insert(session_name, PendingPlayerSession { child: receipt.mailbox_id, cancellation: None });
        debug_assert!(replaced.is_none(), "a staged session is reserved exactly once");
    }

    #[handler(task)]
    fn on_player_session_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<Result<SpawnApplied, SpawnError>, PlayerSessionContinuation>,
    ) {
        let continuation = done.context().clone();
        let Some(pending) = state.pending_sessions.remove(&continuation.session_name) else {
            tracing::warn!(
                target: "aether_game",
                session = %continuation.session_name,
                "game gateway ignored a stale player-session spawn completion",
            );
            if let Ok(applied) = done.output() {
                state.close_live_child(ctx, applied.mailbox_id, &continuation, "stale player session activation");
            }
            done.release_no_reply();
            return;
        };

        match done.output() {
            Err(error) => {
                tracing::warn!(
                    target: "aether_game",
                    session = %continuation.session_name,
                    error = ?error,
                    "player session activation failed",
                );
                ctx.actor::<TcpCapability>().session_close(&state.listener_name, &continuation.session_name);
            }
            Ok(applied) => {
                debug_assert_eq!(pending.child, applied.mailbox_id, "spawn completion must match its reservation");
                if let Some(cancellation) = pending.cancellation {
                    let _ = ctx.send_envelope_tracked(
                        applied.mailbox_id,
                        SessionClosed::ID,
                        &cancellation.encode_into_bytes(),
                    );
                    ctx.actor::<TcpCapability>().session_close(&state.listener_name, &continuation.session_name);
                } else {
                    match ctx.monitor(applied.mailbox_id) {
                        Ok(monitor) => {
                            let replaced = state.sessions.insert(
                                continuation.session_name.clone(),
                                PlayerSessionEntry { child: applied.mailbox_id, _monitor: monitor },
                            );
                            debug_assert!(replaced.is_none(), "a player session becomes live exactly once");
                            let replaced =
                                state.session_by_child.insert(applied.mailbox_id, continuation.session_name.clone());
                            debug_assert!(replaced.is_none(), "a player child supervises exactly one session");
                        }
                        Err(error) => {
                            tracing::warn!(
                                target: "aether_game",
                                session = %continuation.session_name,
                                error = ?error,
                                "player session monitor failed",
                            );
                            state.close_live_child(
                                ctx,
                                applied.mailbox_id,
                                &continuation,
                                "player session supervision failed",
                            );
                        }
                    }
                }
            }
        }
        done.release_no_reply();
    }

    #[handler::single]
    fn on_session_closed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SessionClosed) {
        if !state.is_trusted_tcp_session(ctx, &mail.session_name) {
            return;
        }
        if let Some(entry) = state.sessions.remove(&mail.session_name) {
            state.session_by_child.remove(&entry.child);
            let _ = ctx.send_envelope_tracked(entry.child, SessionClosed::ID, &mail.encode_into_bytes());
            return;
        }
        if let Some(entry) = state.pending_sessions.get_mut(&mail.session_name)
            && entry.cancel(mail.clone())
        {
            let _ = ctx.send_envelope_tracked(entry.child, SessionClosed::ID, &mail.encode_into_bytes());
        }
    }

    #[handler::single]
    fn on_tick_bundle(state: &mut Self::State, ctx: &mut NativeCtx<'_>, bundle: TickBundle) {
        if ctx.source_mailbox() != state.turn_sim_mailbox {
            tracing::warn!(target: "aether_game", "game gateway dropped facts from an unconfigured sender");
            return;
        }

        let bytes = bundle.encode_into_bytes();
        for entry in state.sessions.values() {
            let _ = ctx.send_envelope_tracked(entry.child, TickBundle::ID, &bytes);
        }
    }

    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        if let Some(session_name) = state.session_by_child.remove(&notice.target) {
            state.sessions.remove(&session_name);
        }
    }
}

impl GameGatewayState {
    fn is_at_capacity(&self) -> bool {
        self.sessions.len() + self.pending_sessions.len() >= self.max_active_sessions
    }

    fn close_live_child(
        &self,
        ctx: &mut NativeCtx<'_>,
        child: MailboxId,
        continuation: &PlayerSessionContinuation,
        reason: &str,
    ) {
        let close_notice = SessionClosed {
            session_name: continuation.session_name.clone(),
            peer: continuation.peer.clone(),
            reason: reason.to_owned(),
        };
        let _ = ctx.send_envelope_tracked(child, SessionClosed::ID, &close_notice.encode_into_bytes());
        ctx.actor::<TcpCapability>().session_close(&self.listener_name, &continuation.session_name);
    }

    fn is_trusted_tcp_session(&self, ctx: &NativeCtx<'_>, session_name: &str) -> bool {
        let session =
            ctx.actor::<TcpCapability>().session::<TcpSessionActor>(&self.listener_name, session_name).mailbox_id();
        ctx.source_mailbox() == Some(session)
    }
}

impl PendingPlayerSession {
    fn cancel(&mut self, notice: SessionClosed) -> bool {
        if self.cancellation.is_some() {
            return false;
        }
        self.cancellation = Some(notice);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_capacity(max_active_sessions: usize) -> GameGatewayState {
        GameGatewayState {
            self_mailbox: MailboxId::NONE,
            listener_addr: None,
            listener_name: DEFAULT_LISTENER_NAME.to_owned(),
            turn_sim_mailbox: None,
            interval_nanos: DEFAULT_INTERVAL_NANOS,
            max_active_sessions,
            max_pending_live_bundles: GameGatewayConfig::DEFAULT_MAX_PENDING_LIVE_BUNDLES,
            pending_sessions: HashMap::new(),
            sessions: HashMap::new(),
            session_by_child: HashMap::new(),
        }
    }

    /// Controlled actor-local reducer proof: scheduler-backed loopback tests
    /// below cover the owner/task interaction that promotes the reservation.
    #[test]
    fn pending_session_consumes_capacity_before_live_promotion() {
        let mut state = state_with_capacity(1);
        state
            .pending_sessions
            .insert("conn-0".to_owned(), PendingPlayerSession { child: MailboxId(0x4069), cancellation: None });

        assert!(state.is_at_capacity());
        assert!(state.sessions.is_empty(), "a reservation is not a supervised live session");
        assert!(state.session_by_child.is_empty(), "a reservation has no reverse live index");
    }

    /// Controlled cancellation reducer proof: only the first TCP close is
    /// forwarded by the handler, so a later task completion cannot resurrect
    /// the session or overwrite the reason that won the race.
    #[test]
    fn pending_session_latches_the_first_close_once() {
        let mut pending = PendingPlayerSession { child: MailboxId(0x4069), cancellation: None };
        let first = SessionClosed {
            session_name: "conn-0".to_owned(),
            peer: "127.0.0.1:4069".to_owned(),
            reason: "peer closed".to_owned(),
        };
        let duplicate = SessionClosed { reason: "duplicate reader notice".to_owned(), ..first.clone() };

        assert!(pending.cancel(first));
        assert!(!pending.cancel(duplicate));
        assert_eq!(pending.cancellation.as_ref().map(|notice| notice.reason.as_str()), Some("peer closed"));
    }
}
