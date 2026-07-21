//! Runtime half of the player gateway.

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;

use aether_actor::runtime;
use aether_data::{Kind, MailboxId};
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::{BindListenerResult, GameGatewayCapability, MonitorNotice, SessionClosed, SessionData, TickBundle};
use crate::player::PlayerSessionActor;
use crate::player::session::PlayerSessionConfig;
use aether_tcp::{TcpCapability, TcpNativeExt, TcpSessionActor};

const DEFAULT_LISTENER_NAME: &str = "players";
const DEFAULT_INTERVAL_NANOS: u64 = 1_000_000_000 / 60;

/// Inert-by-default game-listener and authoritative-simulation wiring.
///
/// An active server supplies both `listener_addr` and `turn_sim_mailbox`.
/// The gateway captures its resolved `ctx.self_id()` at init, then actor wiring
/// binds `listener_addr` under `listener_name` and passes that exact mailbox as
/// the tcp consumer.
/// `listener_name` is retained only to address the trusted tcp listener/session
/// topology for outbound writes; it never enters the player wire as a recipient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameGatewayConfig {
    /// Listener address to bind, or `None` to leave the gateway inert.
    pub listener_addr: Option<String>,
    /// Trusted TCP listener topology name used for transport demultiplexing.
    pub listener_name: String,
    /// Exact resolved `TurnSim` mailbox used for intent dispatch and polling.
    pub turn_sim_mailbox: Option<MailboxId>,
    /// Authoritative simulation interval included in player clock beacons.
    pub interval_nanos: u64,
    /// Maximum number of simultaneously supervised player sessions.
    pub max_active_sessions: usize,
    /// Maximum distinct live ticks buffered by each catching-up player session.
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
            turn_sim_mailbox: None,
            interval_nanos: DEFAULT_INTERVAL_NANOS,
            max_active_sessions: Self::DEFAULT_MAX_ACTIVE_SESSIONS,
            max_pending_live_bundles: Self::DEFAULT_MAX_PENDING_LIVE_BUNDLES,
        }
    }
}

pub struct GameGatewayState {
    self_mailbox: MailboxId,
    listener_addr: Option<String>,
    listener_name: String,
    turn_sim_mailbox: Option<MailboxId>,
    interval_nanos: u64,
    max_active_sessions: usize,
    max_pending_live_bundles: usize,
    sessions: HashMap<String, PlayerSessionEntry>,
    session_by_child: HashMap<MailboxId, String>,
}

struct PlayerSessionEntry {
    child: MailboxId,
    _monitor: MonitorHandle,
}

#[runtime]
impl NativeActor for GameGatewayCapability {
    type State = GameGatewayState;
    type Config = GameGatewayConfig;
    const NAMESPACE: &'static str = "aether.game.gateway";

    fn init(config: GameGatewayConfig, ctx: &mut NativeInitCtx<'_>) -> Result<GameGatewayState, BootError> {
        Ok(GameGatewayState {
            self_mailbox: ctx.self_id(),
            listener_addr: config.listener_addr,
            listener_name: config.listener_name,
            turn_sim_mailbox: config.turn_sim_mailbox,
            interval_nanos: config.interval_nanos,
            max_active_sessions: config.max_active_sessions,
            max_pending_live_bundles: config.max_pending_live_bundles,
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

        if state.sessions.len() >= state.max_active_sessions {
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
        let close_notice = SessionClosed {
            session_name: session_name.clone(),
            peer: mail.peer.clone(),
            reason: "player session supervision failed".into(),
        };
        let child = match ctx
            .spawn_child::<PlayerSessionActor>(
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
            .finish()
        {
            Ok(child) => child,
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
        let monitor = match ctx.monitor(child) {
            Ok(monitor) => monitor,
            Err(error) => {
                tracing::warn!(
                    target: "aether_game",
                    session = %session_name,
                    error = ?error,
                    "player session monitor failed",
                );
                let _ = ctx.send_envelope_tracked(child, SessionClosed::ID, &close_notice.encode_into_bytes());
                ctx.actor::<TcpCapability>().session_close(&state.listener_name, &session_name);
                return;
            }
        };

        state.sessions.insert(session_name.clone(), PlayerSessionEntry { child, _monitor: monitor });
        state.session_by_child.insert(child, session_name);
    }

    #[handler::single]
    fn on_session_closed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SessionClosed) {
        if !state.is_trusted_tcp_session(ctx, &mail.session_name) {
            return;
        }
        if let Some(entry) = state.sessions.remove(&mail.session_name) {
            state.session_by_child.remove(&entry.child);
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
    fn is_trusted_tcp_session(&self, ctx: &NativeCtx<'_>, session_name: &str) -> bool {
        let session =
            ctx.actor::<TcpCapability>().session::<TcpSessionActor>(&self.listener_name, session_name).mailbox_id();
        ctx.source_mailbox() == Some(session)
    }
}
