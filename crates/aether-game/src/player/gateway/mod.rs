//! Player gateway identity: one TCP consumer and fact-sink fanout point.

use crate::TickBundle;
use aether_kinds::MonitorNotice;
use aether_tcp::{BindListenerResult, SessionClosed, SessionData};

/// `aether.game.gateway` singleton gateway.
#[actor(singleton)]
pub struct GameGatewayCapability;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;

#[cfg(feature = "runtime")]
pub use runtime::GameGatewayConfig;
