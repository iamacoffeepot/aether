use std::sync::Arc;

use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::mail::outbound::HubOutbound;
use wasmtime::{Engine, Linker};

/// Configuration for `ComponentHostCapability`. `engine` and
/// `linker` are the wasmtime instances every load instantiates
/// against (handed through to the trampoline's
/// `Component::instantiate` call); `hub_outbound` is the egress
/// handle the cap uses for `aether.kinds.changed` announcements
/// after each load. ADR-0021 fan-out is mail-driven post-issue-640
/// — the cap mails subscribe / unsubscribe to `aether.input`
/// rather than mutating shared state.
pub struct ComponentHostConfig {
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<ComponentCtx>>,
    pub hub_outbound: Arc<HubOutbound>,
}
