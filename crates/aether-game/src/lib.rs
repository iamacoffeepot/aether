//! `aether.game` player-gateway cap (ADR-0144, ADR-0145).
//!
//! Target-agnostic game contracts and the trusted player session tier.
//! Two things live here: the tick-native intent / fact vocabulary a
//! simulation and its clients share (`kinds`, re-exported at the crate
//! root), and the trusted
//! server-side boundary that carries it to player clients over the
//! opaque `aether.tcp` transport (`player`).
//!
//! ## Session lineage
//!
//! [`GameGatewayCapability`] (Singleton) is the cap's TCP consumer and
//! fact-sink fanout point. It binds one listener through `aether.tcp`,
//! validates that every inbound session belongs to that listener's
//! lineage, and spawns one [`PlayerSessionActor`] (Instanced) per
//! accepted connection. Trusted `TickBundle` facts arriving from the
//! simulation fan out to those children.
//!
//! ## Wire vocabulary
//!
//! [`PlayerFrame`] is the transport vocabulary, versioned by
//! [`WIRE_VERSION`]. It is recipient-free by construction: an intent or
//! fact frame carries only a kind id plus that kind's encoded payload,
//! so no frame a client sends can name an actor mailbox. The session
//! actor chooses every recipient locally and applies a closed intent
//! allowlist, which is what makes the boundary trusted rather than
//! merely authenticated.
//!
//! ## Crate shape
//!
//! Extracted from `aether-capabilities` (iamacoffeepot/aether#3754) as a
//! per-cap crate of the arc that dissolves the capabilities monolith. It
//! is a leaf: no other capability depends on it, so capabilities keeps no
//! `aether-game` dependency (no facade). It depends downward on
//! `aether-tcp` for the transport its sessions ride.
//!
//! The ADR-0122 identity/runtime split rides the `runtime` feature: the
//! mail kinds, the `PlayerFrame` vocabulary, and the wasm-safe actor
//! identities plus their `HandlesKind` markers compile always-on; the
//! `aether_substrate`-typed runtime half (the gateway's session map and
//! supervision, the per-session frame codec and intent allowlist) is
//! gated so a marker-only wasm guest can address
//! `ctx.actor::<GameGatewayCapability>()` without dragging the substrate
//! through.

extern crate alloc;

mod kinds;
pub mod player;

pub use kinds::*;
#[cfg(feature = "runtime")]
pub use player::GameGatewayConfig;
pub use player::{GameGatewayCapability, PlayerFrame, PlayerSessionActor, WIRE_VERSION};
