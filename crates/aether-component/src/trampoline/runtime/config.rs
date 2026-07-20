//! Init config for the wasm trampoline actor (ADR-0090).

use std::sync::Arc;

use aether_kinds::ComponentCapabilities;
use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use wasmtime::{Engine, Linker, Module};

/// Configuration handed to [`Lifecycle::init`](aether_actor::Lifecycle::init) by the spawn
/// path. Carries the wasmtime engine / linker plus the parsed
/// module bytes; `init` instantiates the `Component` against the
/// trampoline's binding.
pub struct WasmTrampolineConfig {
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<ComponentCtx>>,
    pub module: Module,
    pub registry: Arc<Registry>,
    pub outbound: Arc<HubOutbound>,
    /// Component capabilities parsed from the wasm's
    /// `aether.kinds.inputs` custom section, surfaced through
    /// `LoadResult::Ok.capabilities` at the cap. The trampoline
    /// keeps a handle so it can rehydrate after a replace.
    pub capabilities: ComponentCapabilities,
    /// ADR-0090 (issue 1257): init-config bytes from the
    /// `aether.component.load` mail, handed to the guest's typed
    /// `WasmActor::init` via `Component::instantiate`. Empty means
    /// "no config" — a `Config = ()` guest decodes `&[]` uniformly.
    pub config: Vec<u8>,
    /// ADR-0096: the selected export's actor-type tag
    /// (`mailbox_id_from_name(NAMESPACE)`), threaded through to
    /// `Component::instantiate` so it calls `init_typed_p32`.
    /// `None` instantiates the module's entry type via the legacy
    /// `init_with_config_p32` path — the only type a single-actor
    /// module has. Stored on the trampoline so a later
    /// `ReplaceComponent` rebuilds the same export.
    pub type_tag: Option<u64>,
    /// ADR-0097: every exported type's capability group, parsed once
    /// at load. The trampoline keeps it so a `spawn_child::<Sibling>`
    /// host-fn request can register the spawned sibling's *own*
    /// handler set (looked up by actor-type tag), and so each
    /// spawned sibling carries the same map for its own spawns.
    pub actor_caps: Vec<ActorInputs>,
}
