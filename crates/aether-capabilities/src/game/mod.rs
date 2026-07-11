//! Target-agnostic game contracts and the trusted player session tier.

mod kinds;
pub mod player;

pub use kinds::*;
#[cfg(feature = "runtime")]
pub use player::GameGatewayConfig;
pub use player::{GameGatewayCapability, PlayerFrame, PlayerSessionActor, WIRE_VERSION};
