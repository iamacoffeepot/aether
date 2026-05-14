//! `aether.engine` — engine-management capability cluster (issue 763).
//!
//! P3 (this phase) ships [`proxy::EngineProxy`], the per-engine proxy
//! actor that wraps one outbound RPC connection to a substrate — the
//! bridge core of the forward-model architecture. P4 adds the engines
//! cap (`list` / `spawn` / `terminate`) that supervises a fleet of
//! these proxies and drives `ForwardEnvelope` at them.
//!
//! See issue 763 for the full design.

pub mod proxy;

pub use proxy::EngineProxy;
#[cfg(not(target_arch = "wasm32"))]
pub use proxy::EngineProxyConfig;
