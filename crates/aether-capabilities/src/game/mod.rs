//! Target-agnostic game contracts and the trusted player session tier.

mod kinds;
pub mod player;

pub use kinds::*;
pub use player::{PlayerFrame, PlayerGatewayCapability, PlayerGatewayConfig, PlayerSessionActor, WIRE_VERSION};
