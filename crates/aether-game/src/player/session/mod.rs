//! Per-connection player-session identity.

use crate::{PollResult, TickBundle};
use aether_tcp::{SessionClosed, SessionData};
#[cfg(feature = "runtime")]
use alloc::string::String;

#[cfg(feature = "runtime")]
pub struct PlayerSessionConfig {
    pub listener_name: String,
    pub session_name: String,
    pub peer: String,
    pub turn_sim_mailbox: aether_data::MailboxId,
    pub interval_nanos: u64,
    pub max_pending_live_bundles: usize,
}

/// `aether.game.player.session` actor, one trusted boundary per TCP connection.
#[actor(instanced)]
pub struct PlayerSessionActor;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
