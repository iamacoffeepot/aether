//! Trusted player sessions over the opaque `aether.tcp` transport.

mod frame;
mod gateway;
mod session;

pub use frame::{PlayerFrame, WIRE_VERSION};
pub use gateway::{PlayerGatewayCapability, PlayerGatewayConfig};
pub use session::PlayerSessionActor;

#[cfg(all(test, feature = "runtime"))]
mod tests;
