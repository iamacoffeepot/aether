//! Player gateway identity: one TCP consumer and fact-sink fanout point.

use crate::game::TickBundle;
use crate::tcp::{SessionClosed, SessionData};
use aether_kinds::MonitorNotice;

/// Inert-by-default gateway configuration.
///
/// `turn_sim = None` leaves the gateway registered but unable to create
/// sessions. A configured chassis supplies the authoritative simulation
/// mailbox and its tick interval; that simulation configures this gateway's
/// mailbox as [`crate::game::SimConfig::fact_sink`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerGatewayConfig {
    pub turn_sim: Option<aether_data::MailboxId>,
    pub tick_interval_nanos: u64,
}

impl Default for PlayerGatewayConfig {
    fn default() -> Self {
        Self { turn_sim: None, tick_interval_nanos: 1_000_000_000 / 60 }
    }
}

/// `aether.game.player` singleton gateway.
#[actor(singleton)]
pub struct PlayerGatewayCapability;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
