//! Sender-side peer-addressing facades for loaded components —
//! the "routing" seam of the `aether.component` capability.

use aether_actor::{Addressable, Embedded, ReplyMode, WasmActorMailbox, WasmCtx};
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::NativeActorMailbox;

use super::ComponentHostCapability;
use crate::trampoline::WasmTrampoline;

/// Sender-side facade for FFI guests addressing a loaded peer
/// component through [`ComponentHostCapability`].
///
/// "Sending mail to a loaded component" isn't a SDK primitive — it
/// only exists *because* this cap loaded a wasm component and gave it
/// a trampoline address. So the helper lives here, attached to the
/// cap's FFI mailbox, mirroring `aether_fs::FsMailboxExt`'s
/// cap-owned facade pattern (issue 580).
///
/// `.loaded::<R>(name)` traverses the declared host-to-trampoline edge, then
/// exposes that same physical mailbox under the guest recipient type.
///
/// `R: Addressable<Resolver = Embedded>` is the peer component's actor type,
/// supplied by the caller. The trampoline mailbox is physically an embedded
/// component route, so root, caller-relative, and embedded-many recipients
/// cannot be retyped onto it. Type-checks at the send site —
/// `peer.send::<K>(&mail)` compiles only when `R: HandlesKind<K>`.
pub trait ComponentHostWasmExt {
    /// Resolve a typed peer-component mailbox for the loaded component
    /// named `name`. The resolved handle inherits this handle's ctx binding
    /// (`sender` + inline registry), so its sends stamp the same origin
    /// (issue 1987).
    fn loaded<R: Addressable<Resolver = Embedded>>(&self, name: &str) -> WasmActorMailbox<'_, R>;
}

impl ComponentHostWasmExt for WasmActorMailbox<'_, ComponentHostCapability> {
    fn loaded<R: Addressable<Resolver = Embedded>>(&self, name: &str) -> WasmActorMailbox<'_, R> {
        let trampoline = self.resolve::<WasmTrampoline>(name);
        trampoline.at(trampoline.mailbox_id().0)
    }
}

/// Sender-side facade for native cap-to-cap callers addressing a
/// loaded peer component through [`ComponentHostCapability`]. Same
/// shape as [`ComponentHostWasmExt`] for the native transport — the
/// returned handle inherits the parent mailbox's `'a` binding ref so
/// `.send::<K>(&mail)` dispatches through the same `NativeBinding`
/// without re-threading the ctx.
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub trait ComponentHostNativeExt {
    /// Resolve a typed peer-component mailbox for the loaded component
    /// named `name`.
    fn loaded<R: Addressable<Resolver = Embedded>>(&self, name: &str) -> NativeActorMailbox<'_, R>;
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl ComponentHostNativeExt for NativeActorMailbox<'_, ComponentHostCapability> {
    fn loaded<R: Addressable<Resolver = Embedded>>(&self, name: &str) -> NativeActorMailbox<'_, R> {
        let trampoline = self.resolve::<WasmTrampoline>(name);
        trampoline.at(trampoline.mailbox_id().0)
    }
}

/// The peer verb (iamacoffeepot/aether#4478): addressing a co-hosted
/// component in one call off the receive ctx, so the send site reads the
/// way it thinks — "my host's instance of `R`" — for the cost of one
/// import.
///
/// That phrase is the whole contract. Embeddable means host-agnostic
/// (iamacoffeepot/aether#4479): these verbs promise the peer under
/// *whatever* embedded the caller, and the explicit
/// [`ComponentHostCapability`] route in today's impl is a
/// single-host-era detail — once the ctx carries the parent scope
/// (#4479), the same verbs resolve against it and composite hosts
/// (ADR-0113) get the correct answer with no call-site change.
///
/// `ctx.peer::<R>()` names the default-named instance — what a load with
/// no `name` registers as. A component loaded under an explicit name (or
/// a `replicas` fan-out, whose instances are all `{base}-{index}`) is
/// named at the send site through [`PeerCtxExt::peer_named`], because a
/// bare type cannot identify an instance — load names are runtime facts.
///
/// This trait carries no resolution of its own: both verbs delegate to
/// [`loaded`](ComponentHostWasmExt::loaded), so the agreement the test
/// below pins — bare-type, by-name, and by-carry addressing land on one
/// `MailboxId` — extends to it by construction.
pub trait PeerCtxExt {
    /// The default-named instance of peer component `R` — the mailbox a
    /// nameless load of `R`'s module registers.
    fn peer<R: Addressable<Resolver = Embedded>>(&self) -> WasmActorMailbox<'_, R>;

    /// The instance of peer component `R` loaded under `name`.
    fn peer_named<R: Addressable<Resolver = Embedded>>(&self, name: &str) -> WasmActorMailbox<'_, R>;
}

impl<M: ReplyMode> PeerCtxExt for WasmCtx<'_, M> {
    fn peer<R: Addressable<Resolver = Embedded>>(&self) -> WasmActorMailbox<'_, R> {
        self.peer_named::<R>(R::NAMESPACE)
    }

    // `loaded`'s body, restated over the mailbox primitives rather than
    // called: its trait signature elides the returned lifetime to the
    // borrow of the host handle, which here is a temporary this fn owns.
    // The primitives return the handle's *binding* lifetime, which is the
    // ctx borrow and outlives the return.
    fn peer_named<R: Addressable<Resolver = Embedded>>(&self, name: &str) -> WasmActorMailbox<'_, R> {
        let host = self.actor::<ComponentHostCapability>();
        let trampoline = host.resolve::<WasmTrampoline>(name);
        trampoline.at(trampoline.mailbox_id().0)
    }
}

/// Resolve the [`MailboxId`](aether_data::MailboxId) of the embeddable
/// component loaded under `name`, by folding the instance node
/// `aether.embedded:<name>` (the [`Embedded`]
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
    use aether_actor::Resolve;
    Embedded::resolve(<ComponentHostCapability as Addressable>::resolve(0, ()).0, name, ())
}
