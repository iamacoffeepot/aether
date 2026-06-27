use std::sync::Arc;

use aether_substrate::actor::wasm::component::{Component, ComponentCtx};
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::mail::MailboxId;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use wasmtime::{Engine, Linker, Module};

/// Per-component trampoline **runtime state** (ADR-0122 identity/runtime
/// split — the addressing identity is the distinct ZST
/// [`WasmTrampoline`](crate::trampoline::WasmTrampoline)). Holds the wasm
/// `Component` optionally — `None` means the wasm has been unloaded by
/// `DropComponent` but the trampoline (and its mailbox name) is
/// still alive, ready to be refilled by `ReplaceComponent` or
/// recycled by a future load. Distinction matters: dropping the
/// **component** is a wasm unload that preserves the addressable
/// name; dropping the **trampoline** would kill the actor and
/// tombstone the subname. The cap's `DropComponent` handler does
/// the former; the latter happens at substrate teardown.
pub struct WasmTrampolineState {
    /// `Some` while wasm is loaded; `None` after a `DropComponent`.
    /// Mail arriving in the `None` state warn-drops via the
    /// fallback (the trampoline is just an empty named slot).
    pub(in crate::trampoline::runtime) component: Option<Component>,
    /// Held for [`Self::on_replace_component`] so a fresh
    /// `Component::instantiate` against the same engine + linker
    /// is reachable from the handler.
    pub(in crate::trampoline::runtime) engine: Arc<Engine>,
    pub(in crate::trampoline::runtime) linker: Arc<Linker<ComponentCtx>>,
    pub(in crate::trampoline::runtime) registry: Arc<Registry>,
    pub(in crate::trampoline::runtime) mailer: Arc<Mailer>,
    pub(in crate::trampoline::runtime) outbound: Arc<HubOutbound>,
    /// The trampoline's own mailbox id
    /// (== `MailboxId::from_name(full_name)`). Cached because
    /// `NativeCtx` only exposes `self_id()` via the
    /// `NativeInitCtx` flavour today; storing it here avoids
    /// reaching into `ctx.binding().self_mailbox()` on every
    /// handler call.
    pub(in crate::trampoline::runtime) mailbox: MailboxId,
    /// ADR-0096: the selected export's actor-type tag, or `None`
    /// for the entry type. Held so [`Self::handle_replace`]
    /// re-instantiates the same exported type from the new wasm
    /// and re-reads that type's capability group.
    pub(in crate::trampoline::runtime) type_tag: Option<u64>,
    /// ADR-0097: the resident `Module`, retained so a sibling spawn
    /// re-instantiates it (a cheap `Arc` clone — wasmtime shares the
    /// compiled code) without a re-compile, and refreshed on replace.
    pub(in crate::trampoline::runtime) module: Module,
    /// ADR-0097: every exported type's capability group (see
    /// [`WasmTrampolineConfig::actor_caps`]). A spawned sibling looks
    /// up its own handler set here by actor-type tag.
    pub(in crate::trampoline::runtime) actor_caps: Vec<ActorInputs>,
}
