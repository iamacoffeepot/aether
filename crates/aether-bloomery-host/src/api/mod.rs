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
//! The whole module is runtime-gated (it pulls `aether-substrate` /
//! `aether-capabilities`), so the marker/wasm consumer never links it.

mod dto;
mod runtime;

use aether_actor::actor;

// The handled-kind types the `#[actor(singleton)]` dispatch table references —
// the request ingress, the downstream reply kinds (one typed handler each, so
// the accept-set stays buffered rather than tripping the streaming path a
// broad `#[fallback]` would), and the settlement-notice safety net.
use aether_bloomery::{AdmitResult, QueryResult, ReplayJournalResult};
use aether_capabilities::http::HttpServerRequest;
use aether_kinds::trace::Settled;

use crate::artifacts::GetResult;
use crate::signing::VerifyResult;

/// Addressing identity for the `aether.bloomery.api` capability (ADR-0122).
#[actor(singleton)]
pub struct BloomeryApiCapability;

pub use runtime::ApiCapabilityState;
