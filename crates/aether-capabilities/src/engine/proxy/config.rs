//! Init config for the per-engine proxy (ADR-0090). `EngineProxyConfig`
//! is handed in by the engines cap at `spawn_child`; `HeartbeatParams`
//! is the liveness tuning the cap resolved from its `EngineConfig`.
//! Native-only: the config owns a `std::process::Child` handle.

use aether_data::EngineId;
use std::process::Child;
use std::time::Duration;

/// Init config for `EngineProxy`. `engine_id` is the proxy's
/// engine identity (also the per-instance subname — full address
/// `aether.engine.proxy:<engine_id>`); `rpc_addr` is the
/// substrate's `RpcServerCapability` bind address the proxy dials
/// at init.
///
/// `spawned` is `Some` when the engines cap (`aether.engine`)
/// fork+exec'd the substrate and handed its child handle here —
/// the proxy then owns that process: it retries the startup dial
/// (the substrate may not have bound its port yet), kills it on a
/// failed boot, and SIGKILLs + reaps it on `Drop`. `None` for an
/// adopted / externally-running substrate, whose lifetime the
/// proxy doesn't manage.
///
/// `heartbeat` is the liveness-probe tuning the cap resolved from
/// its [`EngineConfig`](crate::engine::server) (issue 1339). `None`
/// disables the heartbeat (the engine is then only evicted on a
/// connection-close `Bye`, never on a wedge); `Some` arms the
/// timer sidecar.
///
/// `connect_budget` is the total time the startup dial keeps
/// retrying a refused connection while a freshly-forked substrate
/// comes up, resolved from the cap's `EngineConfig`. `Some(d)` caps
/// the retry at `d`; `None` is the wait-forever sentinel (retry
/// until the dial succeeds or hits a terminal error). Only consulted
/// when the proxy forked the substrate (`spawned.is_some()`) — an
/// adopted substrate is dialed once.
pub struct EngineProxyConfig {
    pub engine_id: EngineId,
    pub rpc_addr: String,
    pub spawned: Option<Child>,
    pub heartbeat: Option<HeartbeatParams>,
    pub connect_budget: Option<Duration>,
}

/// Resolved liveness-heartbeat tuning for one proxy (issue 1339).
/// `interval` is the ping cadence; `miss_limit` is how many
/// consecutive unanswered pings mark the engine dead — a small N
/// tolerates a transient hiccup without flapping. Detection latency
/// is `miss_limit × interval`. Built by the engines cap from its
/// `EngineConfig` and handed down via [`EngineProxyConfig`].
#[derive(Clone, Copy, Debug)]
pub struct HeartbeatParams {
    pub interval: Duration,
    pub miss_limit: u32,
}
