use std::sync::Arc;

use aether_kinds::ComponentCapabilities;
use aether_substrate::actor::wasm::component::{Component, ComponentCtx};
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::mail::MailboxId;
use aether_substrate::mail::mailer::Mailer;
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

/// Per-component trampoline. Holds the wasm `Component`
/// optionally — `None` means the wasm has been unloaded by
/// `DropComponent` but the trampoline (and its mailbox name) is
/// still alive, ready to be refilled by `ReplaceComponent` or
/// recycled by a future load. Distinction matters: dropping the
/// **component** is a wasm unload that preserves the addressable
/// name; dropping the **trampoline** would kill the actor and
/// tombstone the subname. The cap's `DropComponent` handler does
/// the former; the latter happens at substrate teardown.
pub struct WasmTrampoline {
    /// `Some` while wasm is loaded; `None` after a `DropComponent`.
    /// Mail arriving in the `None` state warn-drops via the
    /// fallback (the trampoline is just an empty named slot).
    pub(super) component: Option<Component>,
    /// Held for [`Self::on_replace_component`] so a fresh
    /// `Component::instantiate` against the same engine + linker
    /// is reachable from the handler.
    pub(super) engine: Arc<Engine>,
    pub(super) linker: Arc<Linker<ComponentCtx>>,
    pub(super) registry: Arc<Registry>,
    pub(super) mailer: Arc<Mailer>,
    pub(super) outbound: Arc<HubOutbound>,
    /// The trampoline's own mailbox id
    /// (== `MailboxId::from_name(full_name)`). Cached because
    /// `NativeCtx` only exposes `self_id()` via the
    /// `NativeInitCtx` flavour today; storing it here avoids
    /// reaching into `ctx.binding().self_mailbox()` on every
    /// handler call.
    pub(super) mailbox: MailboxId,
    /// ADR-0096: the selected export's actor-type tag, or `None`
    /// for the entry type. Held so [`Self::handle_replace`]
    /// re-instantiates the same exported type from the new wasm
    /// and re-reads that type's capability group.
    pub(super) type_tag: Option<u64>,
    /// ADR-0097: the resident `Module`, retained so a sibling spawn
    /// re-instantiates it (a cheap `Arc` clone — wasmtime shares the
    /// compiled code) without a re-compile, and refreshed on replace.
    pub(super) module: Module,
    /// ADR-0097: every exported type's capability group (see
    /// [`WasmTrampolineConfig::actor_caps`]). A spawned sibling looks
    /// up its own handler set here by actor-type tag.
    pub(super) actor_caps: Vec<ActorInputs>,
}
