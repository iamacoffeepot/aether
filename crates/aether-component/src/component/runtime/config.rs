use std::sync::Arc;

use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::mail::outbound::HubOutbound;
use wasmtime::{Engine, Linker};

use crate::component::ParamProviderRegistry;

/// Composer-supplied construction params for `ComponentHostCapability`
/// (ADR-0156 §3). These are live wasmtime / egress handles the composer
/// hands in at boot, not operator-typable config, so they ride `Params`
/// and the cap's `Config` is `()`. `engine` and `linker` are the wasmtime
/// instances every load instantiates against (handed through to the
/// trampoline's `Component::instantiate` call); `hub_outbound` is the
/// egress handle the cap uses for `aether.kinds.changed` announcements
/// after each load. ADR-0021 fan-out is mail-driven post-issue-640
/// — the cap mails subscribe / unsubscribe to `aether.input`
/// rather than mutating shared state.
pub struct ComponentHostParams {
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<ComponentCtx>>,
    pub hub_outbound: Arc<HubOutbound>,
    /// ADR-0170: the params provider registry this host injects from. The
    /// component host is the container for params injection, so the registry
    /// arrives here — composer-supplied, like the wasmtime handles beside it.
    ///
    /// [`ParamProviderRegistry::with_substrate_facts`] is the standard value;
    /// a chassis that knows facts the substrate does not extends it before
    /// composing, and a duplicate claim aborts boot rather than picking a
    /// winner. Shared `Arc` — the registry is read-only after Compose and
    /// every load reads the same one.
    pub param_providers: Arc<ParamProviderRegistry>,
}
