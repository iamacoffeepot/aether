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

use aether_actor::Actor;
use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::{
    ComponentHostCapability, ComponentHostConfig, FsCapability, HandleCapability, HttpCapability,
    InputCapability, InputConfig, TcpCapability, fs::NamespaceRoots, http::HttpConfig,
    trace::TraceObserverCapability,
};
use aether_data::{Kind, MailboxId as DataMailboxId, mailbox_id_from_name};
use aether_kinds::{Shutdown, Tick};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::{LifecycleDriverConfig, LifecycleGraph};

/// Build the standard single-stage lifecycle config every Tick-driven
/// chassis shares today (ADR-0082 PR 3b): a `Tick` self-loop with a
/// `Quit` escape to a `Shutdown` terminal, relaying `Tick` to
/// `aether.input` so the existing `InputCapability::on_tick` fan-out
/// keeps routing to component subscribers. Headless / `test_bench` /
/// desktop all use this identical shape; a chassis that adds
/// `Render` / `Present` stages (ADR-0082 §11) builds its own graph
/// instead.
///
/// # Panics
/// Panics if the (compile-time-fixed) graph fails to build — it can't,
/// the shape is structurally valid; the `expect` documents the
/// invariant.
#[must_use]
pub fn tick_only_lifecycle_config() -> LifecycleDriverConfig<()> {
    let graph = LifecycleGraph::<()>::builder()
        .state::<Tick, _>(|()| Tick {})
        .next::<Tick>()
        .quit::<Shutdown>()
        .terminal::<Shutdown, _>(|()| Shutdown {})
        .start::<Tick>()
        .build()
        .expect("tick-only lifecycle graph is structurally valid");
    let input_mailbox = DataMailboxId(mailbox_id_from_name(InputCapability::NAMESPACE).0);
    LifecycleDriverConfig {
        graph,
        context: (),
        initial_subscribers: vec![(<Tick as Kind>::ID, input_mailbox)],
    }
}

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

/// Wire the aborter, worker count, and the common caps every full-
/// stack chassis carries. The renderer / window caps each chassis
/// adds after this in `.with_actor::<_>()` chains.
///
/// Boot order is declaration order. ADR-0081 retired the central
/// `LogCapability` — every actor owns its own per-actor log ring; no
/// boot ordering is needed for logging anymore.
pub fn with_common_caps<C: Chassis>(builder: Builder<C>, boot: CommonBoot) -> Builder<C> {
    builder
        .with_aborter(boot.aborter)
        .with_workers(boot.workers)
        .with_actor::<HandleCapability>(())
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
