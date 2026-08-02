//! Init config for the wasm trampoline actor (ADR-0090).

use std::sync::Arc;

use aether_kinds::{ComponentCapabilities, ReplicaIdentity};
use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use wasmtime::{Engine, Linker, Module};

use crate::component::ParamProviderRegistry;

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
    /// ADR-0163 §3 (#3984): the module's raw wasm bytes, retained so
    /// `WasmTrampoline::init` can index an asset load window over its
    /// `aether.asset.*` sections (installed on the `ComponentCtx` before
    /// `Component::instantiate`, closed once `wire` returns) and so a later
    /// `spawn_child::<Sibling>` from the same resident module can index its
    /// own window. Shared `Arc` — the module bytes are indexed, never
    /// mutated.
    pub wasm_bytes: Arc<[u8]>,
    /// ADR-0170: the host's provider registry, retained so this trampoline can
    /// build its params bag at instantiate — and rebuild it for the
    /// replacement module on `replace_component`, whose requests may differ
    /// from the ones loaded — and so a sibling spawned from the same resident
    /// module inherits it.
    pub param_providers: Arc<ParamProviderRegistry>,
    /// ADR-0170: the name this instance registers under, one of the load-time
    /// facts providers read (`LoadContext::instance_name`).
    pub instance_name: String,
    /// ADR-0170: which instance of a `replicas: N` fan-out this is, threaded
    /// from `LoadComponent::replica`. [`ReplicaIdentity::SOLE`] for an
    /// unreplicated load.
    pub replica: ReplicaIdentity,
}
