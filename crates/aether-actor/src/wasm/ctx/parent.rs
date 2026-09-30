//! The typed parent door — [`HasParent`], [`InlineParent`], and the
//! [`WasmCtx::parent`] verb that resolves one (ADR-0114 addressing
//! amendment).
//!
//! An actor's placement is declared in full on its `#[actor(..)]`: `root`
//! and a closed `child_of(..)` list. `ctx.parent()` takes the shape of that
//! declaration. It does not exist for an actor with no `child_of(..)` list,
//! it is infallible for a child-only actor, and it is an `Option` for an
//! actor that is also `root`. A send through the handle compiles only for a
//! kind every declared parent handles.

use core::marker::PhantomData;

use aether_data::{ActorMail, MailboxId};

use super::WasmCtx;
use crate::blob::guest::encode_guest;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::model::{AllHandle, Declared};
use crate::reference::ErasedActorRef;
use crate::wasm::inline::{ChainMode, Registry};

/// An actor that declares a `child_of(..)` list, and so has a parent door.
///
/// `#[actor]` emits the one impl when the list is non-empty, and picks
/// [`Parent`](HasParent::Parent) from `root`: `InlineParent<'a, Self>` for a
/// child-only actor, `Option<InlineParent<'a, Self>>` for an actor that may
/// also be placed at the root. An actor without the impl has no
/// [`WasmCtx::parent`].
pub trait HasParent: Declared + Sized {
    /// `InlineParent<'a, Self>` for a child-only actor,
    /// `Option<InlineParent<'a, Self>>` for an actor that is also `root`.
    type Parent<'a>;

    /// Shape the registry's answer as [`Self::Parent`]. Written by the
    /// expansion; a child-only form expects the parent to be present, because
    /// a type without a `Root` record is refused root placement (ADR-0241
    /// §5) and exists only through a typed inline spawn, which records its
    /// parent before `init`.
    #[doc(hidden)]
    fn __parent(found: Option<InlineParent<'_, Self>>) -> Self::Parent<'_>;
}

/// A typed sendable handle to the parent of an actor `A` in its cluster —
/// what [`WasmCtx::parent`] resolves.
///
/// The parent's type is not known at the call site, because `A` may list
/// several parents in `child_of(..)`, so [`Self::send`] is bounded over the
/// whole list: it compiles only for a kind every declared parent handles.
/// The send routes in place through the cluster membrane (queue + drain),
/// never the scheduler.
pub struct InlineParent<'a, A> {
    id: MailboxId,
    /// The addressing actor's own folded [`MailboxId`] raw value — the "from"
    /// half stamped on the in-place send so the parent's `ctx.sender()`
    /// resolves who sent it.
    sender: u64,
    inline: &'a Registry,
    /// `fn() -> A` rather than `A`: the handle owns no actor state.
    actor: PhantomData<fn() -> A>,
}

impl<'a, A> InlineParent<'a, A> {
    const fn new(id: MailboxId, sender: u64, inline: &'a Registry) -> Self {
        Self { id, sender, inline, actor: PhantomData }
    }
}

impl<A: Declared> InlineParent<'_, A> {
    /// Send `payload` to the parent, checked against every declared parent's
    /// handler set. Routes in place through the cluster membrane and inherits
    /// the handler's in-flight causal chain (ADR-0080 §7).
    pub fn send<K: ActorMail>(&self, payload: &K)
    where
        A::Parents: AllHandle<K>,
    {
        self.inline.route_or_enqueue(self.id.0, K::ID.0, encode_guest(payload), 1, ChainMode::Inherit, self.sender);
    }

    /// The proof this parent resolved to: comparing it with `ctx.sender()`
    /// tells mail from the parent apart from mail from anyone else.
    #[must_use]
    pub const fn reference(&self) -> ErasedActorRef {
        ErasedActorRef::new(self.id)
    }
}

impl<'a, A: HasParent, M: ReplyMode> WasmCtx<'a, A, M> {
    /// ADR-0114 addressing amendment: this actor's parent in the cluster, in
    /// the form its placement fixes — an [`InlineParent`] for a child-only
    /// actor, an `Option` of one for an actor that is also `root` (`None`
    /// when it is placed at the root). An actor that declares no
    /// `child_of(..)` has no parent door: the actor that loaded it is outside
    /// the module, and no ctx proves it (ADR-0230 §3).
    ///
    /// Resolves by lookup over the per-component inline registry, never by
    /// folding.
    #[must_use]
    pub fn parent(&self) -> A::Parent<'a> {
        A::__parent(
            self.inline.parent_of(MailboxId(self.mailbox)).map(|id| InlineParent::new(id, self.mailbox, self.inline)),
        )
    }
}
