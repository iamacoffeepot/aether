//! Player gateway identity: one TCP consumer and fact-sink fanout point.

use crate::game::TickBundle;
use crate::tcp::{BindListenerResult, SessionClosed, SessionData};
use aether_kinds::MonitorNotice;

/// `aether.game.gateway` singleton gateway.
#[actor(singleton)]
pub struct GameGatewayCapability;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;

#[cfg(feature = "runtime")]
pub use runtime::GameGatewayConfig;
