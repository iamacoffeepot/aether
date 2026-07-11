//! Runtime half of the player gateway.

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;

use aether_actor::runtime;
use aether_data::MailboxId;
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::{MonitorNotice, PlayerGatewayCapability, PlayerGatewayConfig, SessionClosed, SessionData, TickBundle};
use crate::game::player::PlayerSessionActor;
use crate::game::player::session::PlayerSessionConfig;
use crate::tcp::{SessionClose, TcpSessionActor};

pub struct PlayerGatewayState {
    turn_sim: Option<MailboxId>,
    tick_interval_nanos: u64,
    next_session: u64,
    sessions: HashMap<MailboxId, PlayerSessionEntry>,
    tcp_by_child: HashMap<MailboxId, MailboxId>,
}

struct PlayerSessionEntry {
    child: MailboxId,
    _monitor: MonitorHandle,
}

#[runtime]
impl NativeActor for PlayerGatewayCapability {
    type State = PlayerGatewayState;
    type Config = PlayerGatewayConfig;
    const NAMESPACE: &'static str = "aether.game.player";

    fn init(config: PlayerGatewayConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<PlayerGatewayState, BootError> {
        Ok(PlayerGatewayState {
            turn_sim: config.turn_sim,
            tick_interval_nanos: config.tick_interval_nanos,
            next_session: 0,
            sessions: HashMap::new(),
            tcp_by_child: HashMap::new(),
        })
    }

    #[handler::single]
    fn on_session_data(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SessionData) {
        let Some(tcp_session) = ctx.source_mailbox() else {
            tracing::warn!(target: "aether_substrate::game", "player gateway dropped session data without a TCP sender");
            return;
        };

        if let Some(entry) = state.sessions.get(&tcp_session) {
            ctx.actor_at::<PlayerSessionActor>(entry.child).send(&mail);
            return;
        }

        let Some(turn_sim) = state.turn_sim else {
            ctx.actor_at::<TcpSessionActor>(tcp_session).send(&SessionClose::default());
            return;
        };

        let subname = format!("session-{}", state.next_session);
        state.next_session = state.next_session.checked_add(1).expect("player gateway session counter overflowed");
        let close_notice = SessionClosed {
            session_name: mail.session_name.clone(),
            peer: mail.peer.clone(),
            reason: "player session supervision failed".into(),
        };
        let child = match ctx
            .spawn_child::<PlayerSessionActor>(
                Subname::Named(&subname),
                PlayerSessionConfig {
                    tcp_session,
                    turn_sim,
                    tick_interval_nanos: state.tick_interval_nanos,
                    session_name: mail.session_name.clone(),
                    peer: mail.peer.clone(),
                },
            )
            .after_init(mail)
            .finish()
        {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(
                    target: "aether_substrate::game",
                    session = %subname,
                    error = ?error,
                    "player session spawn failed",
                );
                ctx.actor_at::<TcpSessionActor>(tcp_session).send(&SessionClose::default());
                return;
            }
        };
        let monitor = match ctx.monitor(child) {
            Ok(monitor) => monitor,
            Err(error) => {
                tracing::warn!(
                    target: "aether_substrate::game",
                    session = %subname,
                    error = ?error,
                    "player session monitor failed",
                );
                ctx.actor_at::<PlayerSessionActor>(child).send(&close_notice);
                ctx.actor_at::<TcpSessionActor>(tcp_session).send(&SessionClose::default());
                return;
            }
        };

        state.sessions.insert(tcp_session, PlayerSessionEntry { child, _monitor: monitor });
        state.tcp_by_child.insert(child, tcp_session);
    }

    #[handler::single]
    fn on_session_closed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SessionClosed) {
        let Some(tcp_session) = ctx.source_mailbox() else {
            return;
        };
        if let Some(entry) = state.sessions.get(&tcp_session) {
            ctx.actor_at::<PlayerSessionActor>(entry.child).send(&mail);
        }
    }

    #[handler::single]
    fn on_tick_bundle(state: &mut Self::State, ctx: &mut NativeCtx<'_>, bundle: TickBundle) {
        if ctx.source_mailbox() != state.turn_sim {
            tracing::warn!(target: "aether_substrate::game", "player gateway dropped facts from an unconfigured sender");
            return;
        }
        ctx.fanout(state.sessions.values().map(|entry| entry.child), &bundle);
    }

    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        if let Some(tcp_session) = state.tcp_by_child.remove(&notice.target) {
            state.sessions.remove(&tcp_session);
        }
    }
}
