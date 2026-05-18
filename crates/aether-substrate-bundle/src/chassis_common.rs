//! Shared `Builder` boot fragments for the desktop and headless
//! chassis. Both `Chassis::build` impls pre-extraction wired the same
//! 10-cap base (handle, log, trace, input, component-host, fs, http,
//! tcp + the aborter + worker count) and the same optional RPC
//! server tail, with only their renderer + window stack differing.
//! Qodana flagged the parallel chains as duplicated code; this module
//! pulls the shared scaffolding out so each chassis declares only
//! the parts that genuinely differ.
//!
//! The hub and test-bench chassis don't share this base (hub is a
//! minimal RPC-only chassis, test-bench drives a loopback), so the
//! helper module stays scoped to the two full-stack chassis.

use std::net::SocketAddr;
use std::sync::Arc;

use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::{
    ComponentHostCapability, ComponentHostConfig, FsCapability, HandleCapability, HttpCapability,
    InputCapability, InputConfig, LogCapability, TcpCapability, fs::NamespaceRoots,
    http::HttpConfig, trace::TraceObserverCapability,
};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::runtime::lifecycle::FatalAborter;

/// Args every full-stack chassis hands to [`with_common_caps`]. Kept
/// as a flat struct (no defaults) so an added cap forces the chassis
/// builders to acknowledge it.
pub struct CommonBoot {
    pub aborter: Arc<dyn FatalAborter>,
    pub workers: Option<usize>,
    pub input_config: InputConfig,
    pub component_host_config: ComponentHostConfig,
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConfig,
}

/// Wire the aborter, worker count, and the 10 caps every full-stack
/// chassis carries. The renderer / window caps each chassis adds
/// after this in `.with_actor::<_>()` chains.
///
/// Boot order matters — log first so other capabilities' boot tracing
/// routes through the log capture.
pub fn with_common_caps<C: Chassis>(builder: Builder<C>, boot: CommonBoot) -> Builder<C> {
    builder
        .with_aborter(boot.aborter)
        .with_workers(boot.workers)
        .with_actor::<HandleCapability>(())
        .with_actor::<LogCapability>(())
        .with_actor::<TraceObserverCapability>(())
        .with_actor::<InputCapability>(boot.input_config)
        .with_actor::<ComponentHostCapability>(boot.component_host_config)
        .with_actor::<FsCapability>(boot.namespace_roots)
        .with_actor::<HttpCapability>(boot.http)
        .with_actor::<TcpCapability>(())
}

/// Issue 763 P2: boot the RPC server only when `rpc_addr` is set,
/// mirroring the hub chassis. Substrate becomes an RPC server peer
/// that a hub (or any client) connects out to. `engine_name`
/// identifies the chassis profile in the `HelloAck` peer-kind.
pub fn maybe_with_rpc_server<C: Chassis>(
    builder: Builder<C>,
    rpc_addr: Option<SocketAddr>,
    engine_name: &str,
) -> Builder<C> {
    let Some(rpc_addr) = rpc_addr else {
        return builder;
    };
    builder.with_actor::<RpcServerCapability>(RpcServerConfig {
        bind_addr: rpc_addr.to_string(),
        peer_kind: PeerKind::Substrate {
            engine_name: engine_name.into(),
            engine_version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![],
        },
    })
}
