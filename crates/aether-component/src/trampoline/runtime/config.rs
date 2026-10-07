//! Init config for the wasm trampoline actor (ADR-0090).

use std::sync::Arc;

use aether_kinds::ComponentCapabilities;
use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::actor::wasm::module::{Module, ModuleCache};
use aether_substrate::mail::outbound::HubOutbound;
use wasmtime::{Engine, Linker};

/// Configuration handed to [`Lifecycle::init`](aether_actor::Lifecycle::init) by the spawn
/// path. Carries the wasmtime engine / linker plus the checked-in
/// module; `init` instantiates the `Component` against the
/// trampoline's binding.
pub struct WasmTrampolineConfig {
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<ComponentCtx>>,
    /// The module this trampoline instantiates: its compiled code, its
    /// manifest (every exported type's capability group, read by a
    /// `spawn_child::<Sibling>` request for the spawned sibling's own
    /// handler set, and its asset catalog and blobs). Holding it keeps the
    /// engine module cache's entry for this content hash alive (ADR-0240 D5,
    /// ADR-0241 §2), so a sibling spawn or a same-hash load reuses it until
    /// every holder drops, and the guest reads its assets from it in every
    /// hook (ADR-0250).
    pub module: Module,
    /// The engine's one module cache, through which a republish's prepare
    /// checks its candidate module in (ADR-0241 §2).
    pub modules: ModuleCache,
    pub outbound: Arc<HubOutbound>,
    /// Component capabilities parsed from the wasm's
    /// `aether.kinds.inputs` custom section, surfaced through
    /// `LoadResult::Ok.capabilities` at the cap. The trampoline
    /// keeps a handle so it can rehydrate after a replace.
    pub capabilities: ComponentCapabilities,
    /// ADR-0090 (issue 1257): init-config bytes from the
    /// `aether.component.load` mail, handed to the guest's typed
    /// `WasmActor::init` via `Component::instantiate`. Empty means
    /// "no config" — a `Config = ()` guest decodes `&[]` uniformly. Stored
    /// on the trampoline, so a republish that supplies no config builds its
    /// candidate from it (ADR-0241 §7).
    pub config: Vec<u8>,
    /// ADR-0096: the selected export's actor-type tag
    /// (`ActorId::singleton(NAMESPACE)`), threaded through to
    /// `Component::instantiate` so it calls `init_typed_p32`.
    /// `None` instantiates the module's entry type via the legacy
    /// `init_with_config_p32` path — the only type a single-actor
    /// module has. Stored on the trampoline so a republish's prepare
    /// rebuilds the same export.
    pub type_tag: Option<u64>,
}
