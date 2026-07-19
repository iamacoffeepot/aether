//! `aether.engine` — engine-management capability cluster (issue 763).
//!
//! - [`EngineProxy`] (P3) — the per-engine proxy actor that
//!   wraps one outbound RPC connection to a substrate; the bridge core
//!   of the forward-model architecture.
//! - [`EngineServer`] (P4) — the engines cap (`list` / `spawn`
//!   / `terminate`) that supervises a fleet of proxies, fork+execing
//!   substrates and connecting a proxy to each.
//!
//! See issue 763 for the full design.

// The single owner of the `aether.engine` mailbox name: the real hub-side
// `EngineServer` and the `#[cfg(test)]` `EngineCapSink` stand-in that
// impersonates it in the proxy's round-trip tests claim the same identity, so
// both `impl`s reference this one const (via a `use`, the `trampoline` /
// `EMBEDDED_SCOPE` idiom) instead of re-typing the literal — the test double
// cannot drift from the cap it stands in for. Lives in the cap-cluster root so
// both the `server` and `proxy` descendants can see it; always-on because the
// always-on identity `Addressable` reads it. Enforced by the
// duplicate-`NAMESPACE` source invariant (tests/source_invariants.rs).
const ENGINE_NAMESPACE: &str = "aether.engine";

pub mod kinds;
mod proxy;
mod server;
#[cfg(feature = "runtime")]
mod store;

pub use proxy::EngineProxy;
#[cfg(not(target_family = "wasm"))]
pub use proxy::EngineProxyConfig;
pub use server::EngineServer;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub use server::{EngineConfig, EngineConfigLayer, EngineOverlay};
