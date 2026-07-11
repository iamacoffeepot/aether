//! Per-connection player-session identity.

use crate::game::{PollResult, TickBundle};
use crate::tcp::{SessionClosed, SessionData};
#[cfg(feature = "runtime")]
use alloc::string::String;

#[cfg(feature = "runtime")]
pub struct PlayerSessionConfig {
    pub tcp_session: aether_data::MailboxId,
    pub turn_sim: aether_data::MailboxId,
    pub tick_interval_nanos: u64,
    pub session_name: String,
    pub peer: String,
}

/// `aether.game.player.session` actor, one trusted boundary per TCP connection.
#[actor(instanced)]
pub struct PlayerSessionActor;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
