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
    pub component: Option<Component>,
    /// Held for [`Self::handle_replace`] so a fresh
    /// `Component::instantiate` against the same engine + linker
    /// is reachable from the handler.
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<ComponentCtx>>,
    pub registry: Arc<Registry>,
    pub mailer: Arc<Mailer>,
    pub outbound: Arc<HubOutbound>,
    /// The trampoline's own mailbox id
    /// (== `MailboxId::from_name(full_name)`). Cached because
    /// `NativeCtx` only exposes `self_id()` via the
    /// `NativeInitCtx` flavour today; storing it here avoids
    /// reaching into `ctx.binding().self_mailbox()` on every
    /// handler call.
    pub mailbox: MailboxId,
    /// ADR-0096: the selected export's actor-type tag, or `None`
    /// for the entry type. Held so [`Self::handle_replace`]
    /// re-instantiates the same exported type from the new wasm
    /// and re-reads that type's capability group.
    pub type_tag: Option<u64>,
    /// ADR-0097: the resident `Module`, retained so a sibling spawn
    /// re-instantiates it (a cheap `Arc` clone — wasmtime shares the
    /// compiled code) without a re-compile, and refreshed on replace.
    pub module: Module,
    /// ADR-0097: every exported type's capability group (see
    /// [`super::WasmTrampolineConfig::actor_caps`]). A spawned sibling looks
    /// up its own handler set here by actor-type tag.
    pub actor_caps: Vec<ActorInputs>,
    /// ADR-0163 §3 (#3984): the resident module's raw wasm bytes, retained
    /// so a `spawn_child::<Sibling>` from this module can index its own
    /// asset load window, and refreshed on replace. Shared `Arc` — indexed,
    /// never mutated.
    pub wasm_bytes: Arc<[u8]>,
}
