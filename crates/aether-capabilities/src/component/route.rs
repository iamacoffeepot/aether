//! Sender-side peer-addressing facades for loaded components —
//! the "routing" seam of the `aether.component` capability.

use aether_actor::{Addressable, WasmActorMailbox};
#[cfg(not(target_family = "wasm"))]
use aether_substrate::actor::native::NativeActorMailbox;

use super::ComponentHostCapability;
use crate::trampoline::WasmTrampoline;

/// Sender-side facade for FFI guests addressing a loaded peer
/// component through [`ComponentHostCapability`].
///
/// "Sending mail to a loaded component" isn't a SDK primitive — it
/// only exists *because* this cap loaded a wasm component and gave it
/// a trampoline address. So the helper lives here, attached to the
/// cap's FFI mailbox, mirroring [`crate::fs::FsMailboxExt`]'s
/// cap-owned facade pattern (issue 580).
///
/// `.loaded::<R>(name)` resolves a typed peer handle. The trampoline
/// prefix lives in exactly one place in the workspace —
/// [`WasmTrampoline::NAMESPACE`] (issue 654) — and this method reads
/// from it, so a future rename of the convention touches one constant
/// and propagates everywhere.
///
/// `R: Addressable` is the peer's actor type, supplied by the caller (same
/// as today's `WasmCtx::resolve_actor` surface). Type-checks at the
/// send site — `peer.send::<K>(&mail)` compiles only when
/// `R: HandlesKind<K>`.
pub trait ComponentHostWasmExt {
    /// Resolve a typed peer-component mailbox for the loaded component
    /// named `name`. The full mailbox address is
    /// `format!("{}:{}", WasmTrampoline::NAMESPACE, name)`. The resolved
    /// handle inherits this handle's ctx binding (`sender` + inline
    /// registry), so its sends stamp the same origin (issue 1987).
    fn loaded<R: Addressable>(&self, name: &str) -> WasmActorMailbox<'_, R>;
}

impl ComponentHostWasmExt for WasmActorMailbox<'_, ComponentHostCapability> {
    fn loaded<R: Addressable>(&self, name: &str) -> WasmActorMailbox<'_, R> {
        self.resolve_peer_scoped::<R>(WasmTrampoline::NAMESPACE, name)
    }
}

/// Sender-side facade for native cap-to-cap callers addressing a
/// loaded peer component through [`ComponentHostCapability`]. Same
/// shape as [`ComponentHostWasmExt`] for the native transport — the
/// returned handle inherits the parent mailbox's `'a` binding ref so
/// `.send::<K>(&mail)` dispatches through the same `NativeBinding`
/// without re-threading the ctx.
#[cfg(not(target_family = "wasm"))]
pub trait ComponentHostNativeExt {
    /// Resolve a typed peer-component mailbox for the loaded component
    /// named `name`. The full mailbox address is
    /// `format!("{}:{}", WasmTrampoline::NAMESPACE, name)`.
    fn loaded<R: Addressable>(&self, name: &str) -> NativeActorMailbox<'_, R>;
}

#[cfg(not(target_family = "wasm"))]
impl ComponentHostNativeExt for NativeActorMailbox<'_, ComponentHostCapability> {
    fn loaded<R: Addressable>(&self, name: &str) -> NativeActorMailbox<'_, R> {
        self.resolve_peer_scoped::<R>(WasmTrampoline::NAMESPACE, name)
    }
}

/// Resolve the [`MailboxId`](aether_data::MailboxId) of the embeddable
/// component loaded under `name`, by folding the instance node
/// `aether.embedded:<name>` (the [`Embedded`](aether_actor::Embedded)
/// resolver) onto the `aether.component` host cap's carry (ADR-0099 §5/§6,
/// ADR-0119).
///
/// This is the by-name carry-supplier. `aether-actor`'s `Embedded` resolver
/// owns the fold and the reserved scope
/// ([`EMBEDDED_SCOPE`](aether_actor::EMBEDDED_SCOPE)); this fn supplies the
/// `aether.component` carry, read only from its owner
/// [`ComponentHostCapability`]. Equal by construction to a component's own
/// `type Resolver = Embedded` and to the by-name verb
/// [`loaded::<R>(name)`](ComponentHostWasmExt::loaded), so bare-type and
/// by-name addressing agree. Available on every target — a wasm peer resolves
/// an embeddable the same way a native one does, no transport branch
/// (ADR-0029 client-side no-lookup).
#[must_use]
pub fn resolve_embedded(name: &str) -> aether_data::MailboxId {
    use aether_actor::{Addressable, Embedded, Resolve};
    Embedded::resolve(
        <ComponentHostCapability as Addressable>::resolve(0, ()).0,
        name,
        (),
    )
}
