//! Typed cluster-child addressing — [`InlineChild`] and the
//! [`WasmCtx::child_as`] / [`WasmCtx::sibling_as`] verbs that resolve one.
//!
//! The typed counterpart of `super::relative`: a
//! [`RelativeMailbox`](super::RelativeMailbox) is positional and therefore
//! type-erased, while an [`InlineChild<C>`] names the child type it addresses,
//! so every send through it is checked against `C`'s handler set.

use core::fmt::{Debug, Formatter, Result as FmtResult};
use core::marker::PhantomData;

use aether_data::{ActorMail, MailboxId};

use super::{ActorTypeTag, WasmCtx};
use crate::model::ctx::reply_mode::ReplyMode;
use crate::model::{Addressable, HandlesKind};
use crate::reference::ErasedActorRef;

/// A typed sendable handle to an inline child of type `C` — what
/// [`WasmCtx::spawn_inline_child`] hands back, and what
/// [`WasmCtx::child_as`] / [`WasmCtx::sibling_as`] resolve.
///
/// It wraps the child's alias [`MailboxId`] and nothing else; the `C` rides
/// along as a `PhantomData<fn() -> C>` (covariant, `Send`/`Sync` regardless of
/// `C`, and no drop or auto-trait obligation to the child's own state). What
/// the parameter buys is [`Self::send`]'s `C: HandlesKind<K>` bound: sending a
/// child a kind it declares no `#[handler]` for is an `E0277` at the call site
/// instead of a substrate warn-drop — or, for a child with a `#[fallback]`,
/// instead of nothing at all.
///
/// A parent keeping children of several types erases the handle with
/// [`Self::erase`] and keeps the [`ErasedActorRef`] it yields — still a proof,
/// minus the type, and what a send or `despawn_inline_child` takes.
/// [`Self::id`] remains as the key for a registry lookup, never as a send or
/// despawn target: every by-id send takes a proof.
pub struct InlineChild<C> {
    id: MailboxId,
    /// `fn() -> C` rather than `C`: the handle owns no child state, so it must
    /// not inherit `C`'s auto-traits or drop glue.
    child: PhantomData<fn() -> C>,
}

// Derived `Clone` / `Copy` would bound on `C: Clone` / `C: Copy`, which the
// child actor type has no reason to satisfy — the handle is one `MailboxId`.
impl<C> Clone for InlineChild<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for InlineChild<C> {}

impl<C> PartialEq for InlineChild<C> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<C> Eq for InlineChild<C> {}

impl<C> Debug for InlineChild<C> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_tuple("InlineChild").field(&self.id).finish()
    }
}

impl<C: Addressable> InlineChild<C> {
    /// Wrap a resolved alias as a typed handle. Crate-private on purpose: the
    /// two sanctioned ways to obtain one are a spawn (the type is named at the
    /// call) and a tag-checked re-lookup ([`WasmCtx::child_as`] /
    /// [`WasmCtx::sibling_as`]), so a handle can never claim a type the
    /// registry does not record.
    pub(super) const fn new(id: MailboxId) -> Self {
        Self { id, child: PhantomData }
    }

    /// Send `payload` to this child, checked against `C`'s handler set.
    ///
    /// Routes exactly as [`WasmCtx::send_to`] does — through the inline
    /// registry's cluster router, in place through the membrane for a resident
    /// child, inheriting the handler's causal chain (ADR-0080 §7).
    pub fn send<K: ActorMail, A, M: ReplyMode>(&self, ctx: &mut WasmCtx<'_, A, M>, payload: &K)
    where
        C: HandlesKind<K>,
    {
        ctx.send_to(self.erase(), payload);
    }

    /// This child as an [`ErasedActorRef`] — the ADR-0230 §3 spawn-result door
    /// for a heterogeneous child set. A parent keeping children of several
    /// types keeps proofs, not positions: the erased reference drops `C` but
    /// keeps the fact that the spawn registered the child, so it is a valid
    /// target for [`WasmCtx::send_to`] and a key for a table of children.
    ///
    /// Consumed by [`Self::send`] above, by `aether-widget`'s panel spawn,
    /// and by its composite node's spawn. The *reference* erasure: unrelated
    /// to the ctx reply-mode `erase()` the native `#[actor]` expansion emits.
    #[must_use]
    pub const fn erase(self) -> ErasedActorRef {
        ErasedActorRef::new(self.id)
    }

    /// This child's alias [`MailboxId`] — the key a registry lookup takes. A
    /// send or `despawn_inline_child` takes [`Self::erase`]'s proof instead.
    #[must_use]
    pub const fn id(&self) -> MailboxId {
        self.id
    }

    /// Whether `source` is this child — the identity check a parent makes
    /// against `ctx.sender()`'s position when attributing an inbound reply.
    /// Comparing against a held handle rather than a loose stored id is what
    /// keeps a stale-after-despawn comparison from silently matching a reused
    /// slot.
    #[must_use]
    pub fn matches(&self, source: MailboxId) -> bool {
        self.id == source
    }
}

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// The typed form of [`WasmCtx::child`]: this actor's inline child whose
    /// subname is `name`, as an [`InlineChild<C>`], or `None` when no such
    /// child resides **or** the resident one is not a `C`.
    ///
    /// The type half is answered from the registry's recorded
    /// [`ActorTypeTag`], stamped when the child was installed, so a wrong-type
    /// lookup is `None` rather than a live handle to the wrong actor — and the
    /// handle is re-derivable after a rehydrate, where a `MailboxId` cached in
    /// a field is not.
    #[must_use]
    pub fn child_as<C: Addressable>(&self, name: &str) -> Option<InlineChild<C>> {
        self.typed_child::<C>(self.inline.child_of(MailboxId(self.mailbox), name)?)
    }

    /// The typed form of [`WasmCtx::sibling`]: the child of this actor's
    /// parent whose subname is `name`, as an [`InlineChild<C>`], or `None`
    /// when this actor has no recorded parent, no such sibling resides, or the
    /// resident one is not a `C`. Same recorded-[`ActorTypeTag`] check
    /// [`Self::child_as`] makes.
    #[must_use]
    pub fn sibling_as<C: Addressable>(&self, name: &str) -> Option<InlineChild<C>> {
        self.typed_child::<C>(self.inline.sibling_of(MailboxId(self.mailbox), name)?)
    }

    /// Admit `id` as an [`InlineChild<C>`] only when the registry records it
    /// as a `C`. The one place the tag equality lives, shared by both verbs.
    fn typed_child<C: Addressable>(&self, id: MailboxId) -> Option<InlineChild<C>> {
        (self.inline.actor_type_tag(id)? == ActorTypeTag::of::<C>()).then(|| InlineChild::new(id))
    }
}
