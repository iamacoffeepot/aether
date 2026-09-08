//! The `aether.bloomery.api` capability — the REST control ingress (ADR-0149
//! §Packaging, issue #3498).
//!
//! A native `aether.http.server`-mounted router that lets an operator drive a
//! bloom end-to-end from `curl` — stage workpieces, shape and seal drafts,
//! supersede, and read the live blooms / view document / journal / artifacts —
//! with no typed-mail RPC vocabulary. It is the concrete form of the operator
//! surface ADR-0149 names, mounted on [`BloomeryChassis`](crate::BloomeryChassis)
//! alongside the `aether.http.server` ingress cap (ADR-0108); `RpcServerCapability`
//! stays mounted unchanged for fleet plumbing.
//!
//! The whole module is runtime-gated (it pulls `aether-substrate` and the
//! cap crates), so the marker/wasm consumer never links it.

pub(crate) mod dto;
mod runtime;

// The commission CLI talks the same REST hex spelling the control API renders.
pub(crate) use runtime::hex;

use aether_actor::actor;

#[cfg(feature = "github")]
use crate::bloomery::LatestDoctorReport;

/// Addressing identity for the `aether.bloomery.api` capability (ADR-0122).
#[actor(singleton, root)]
pub struct BloomeryApiCapability;

pub use dto::{MemberProjection, SealRequest};
pub use runtime::{ApiCapabilityState, ApiParams};
