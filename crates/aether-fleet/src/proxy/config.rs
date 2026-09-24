//! Init config for the per-engine proxy (ADR-0090). `FleetProxyConfig`
//! is handed in by the engines cap at `spawn_child`; `HeartbeatParams`
//! is the liveness tuning the cap resolved from its `FleetConfig`.
//! Native-only: the config owns a `std::process::Child` handle.

use super::reap::terminate_child_group;
use aether_data::EngineId;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

/// Init config for `FleetProxy`. `engine_id` is the proxy's
/// engine identity (also the per-instance subname — full address
/// `aether.fleet.proxy:<engine_id>`); `target` names the substrate the
/// proxy dials at init and whether the proxy owns its process.
///
/// `heartbeat` is the liveness-probe tuning the cap resolved from
/// its [`FleetConfig`](crate::FleetConfig) (issue 1339). `None`
/// disables the heartbeat (the engine is then only evicted on a
/// connection-close `Bye`, never on a wedge); `Some` arms the
/// timer sidecar.
///
/// `connect_budget` is the total time the startup dial keeps waiting
/// for a freshly-forked substrate to report its port and retrying a
/// refused connection to it, resolved from the cap's `FleetConfig`.
/// `Some(d)` caps the wait at `d`; `None` is the wait-forever sentinel
/// (wait until the dial succeeds, the child exits, or a terminal error).
/// Only consulted for [`ProxyTarget::Forked`] — an adopted substrate is
/// dialed once.
pub struct FleetProxyConfig {
    pub engine_id: EngineId,
    pub target: ProxyTarget,
    pub heartbeat: Option<HeartbeatParams>,
    pub connect_budget: Option<Duration>,
}

/// The substrate a proxy dials at init.
pub enum ProxyTarget {
    /// An adopted / externally-running substrate at `rpc_addr`, whose
    /// lifetime the proxy doesn't manage. Dialed once.
    Adopted { rpc_addr: String },
    /// A substrate the engines cap (`aether.fleet`) fork+exec'd on port
    /// `0`, handing its child here. The proxy owns that process: it dials
    /// only the port the child reports through `port_file` while the child
    /// is alive (issue 6503), kills it on a failed boot, and terminates +
    /// reaps its process group on `Drop`.
    Forked { child: Child, port_file: PathBuf },
}

impl Drop for FleetProxyConfig {
    /// Terminate + reap a forked child `init` never took: a spawn refused
    /// before `init` runs (a declared dependency that is not live) drops the config
    /// with the child still in it, and nothing else owns that process.
    /// `init` takes the child into the proxy state, so a config dropped
    /// after a successful `init` holds nothing.
    fn drop(&mut self) {
        if let ProxyTarget::Forked { child, .. } = &mut self.target {
            terminate_child_group(child);
        }
    }
}

/// Resolved liveness-heartbeat tuning for one proxy (issue 1339).
/// `interval` is the ping cadence; `miss_limit` is how many
/// consecutive unanswered pings mark the engine dead — a small N
/// tolerates a transient hiccup without flapping. Detection latency
/// is `miss_limit × interval`. Built by the engines cap from its
/// `FleetConfig` and handed down via [`FleetProxyConfig`].
#[derive(Clone, Copy, Debug)]
pub struct HeartbeatParams {
    pub interval: Duration,
    pub miss_limit: u32,
}
