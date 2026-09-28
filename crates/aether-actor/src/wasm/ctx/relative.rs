//! Cluster-relative addressing — [`RelativeMailbox`] and the
//! [`WasmCtx::parent`] / [`WasmCtx::child`] / [`WasmCtx::sibling`] verbs
//! that resolve one (ADR-0114 addressing amendment).

use aether_data::{ActorMail, MailboxId};

use super::WasmCtx;
use crate::blob::guest::encode_guest;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::reference::ErasedActorRef;
use crate::wasm::inline::{ChainMode, Registry};

/// A type-erased sendable handle to a cluster relative — the parent,
/// a sibling, or a child of the addressing actor (ADR-0114 addressing
/// amendment). Returned by [`WasmCtx::parent`] / [`WasmCtx::sibling`] /
/// [`WasmCtx::child`], it wraps the relative's resolved [`MailboxId`] (looked
/// up in the per-component inline registry, never folded) plus the registry
/// the send routes through.
///
/// Unlike an [`ActorRef<R>`](crate::ActorRef) this carries no receiver type and no
/// `R: HandlesKind<K>` bound — relative addressing is positional, so the
/// target's handler set is not known at the call site. The send routes through
/// the inline registry's cluster router: a cluster-member recipient (which a
/// resolved relative always is) dispatches in place via the queue + drain,
/// never the scheduler.
///
/// Reach for this when the position is what you know and the type genuinely
/// is not — a parent hop, a forwarding interposer, a subname read off runtime
/// data. When the child type *is* known, take the typed handle instead:
/// [`WasmCtx::child_as`] / [`WasmCtx::sibling_as`] answer the same lookup as
/// an [`InlineChild<C>`](super::InlineChild) whose sends are checked against
/// `C`'s handler set.
pub struct RelativeMailbox<'a> {
    pub(super) id: MailboxId,
    /// The addressing actor's own folded [`MailboxId`] raw value — the "from"
    /// half stamped on the in-place send so the relative recipient's
    /// `ctx.sender()` resolves who sent it. Set by
    /// [`WasmCtx::parent`] / [`WasmCtx::child`] / [`WasmCtx::sibling`] to the
    /// resolving ctx's `mailbox`.
    sender: u64,
    inline: &'a Registry,
}

impl RelativeMailbox<'_> {
    /// The proof this relative resolved to — the by-tag counterpart of
    /// [`InlineChild::erase`](super::InlineChild::erase). The relative came
    /// from a lookup in the per-component inline registry, so it proves what
    /// `ctx.child_as::<C>(name)?.erase()` proves, for a spawner that cannot
    /// name `C` — comparing this proof with `ctx.sender()` tells one lane
    /// from another when a spawner routes by type tag.
    #[must_use]
    pub const fn reference(&self) -> ErasedActorRef {
        ErasedActorRef::new(self.id)
    }

    /// Resolve a sendable handle to this relative's inline child whose
    /// subname is `name`, preserving the original addresser for any send
    /// through the returned handle. This is the multi-hop continuation of
    /// [`WasmCtx::child`].
    #[must_use]
    pub fn child(&self, name: &str) -> Option<Self> {
        let id = self.inline.child_of(self.id, name)?;
        Some(RelativeMailbox { id, sender: self.sender, inline: self.inline })
    }

    /// Send `payload` to this relative, routed in place through the cluster
    /// membrane (queue + drain) — no scheduler hop. Inherits the handler's
    /// in-flight causal chain (the default, ADR-0080 §7); the local path
    /// carries no host trace ids, so the flag is moot for an in-cluster
    /// send.
    pub fn send<K: ActorMail>(&self, payload: &K) {
        self.inline.route_or_enqueue(self.id.0, K::ID.0, encode_guest(payload), 1, ChainMode::Inherit, self.sender);
    }

    /// Fire-and-forget send to this relative (ADR-0080 §7 detach signal).
    /// In-cluster the recipient dispatches in place regardless; the detach
    /// flag rides through only on the cross-cluster fallback path, which a
    /// resolved relative never takes.
    pub fn send_detached<K: ActorMail>(&self, payload: &K) {
        self.inline.route_or_enqueue(self.id.0, K::ID.0, encode_guest(payload), 1, ChainMode::Detached, self.sender);
    }
}

impl<'a, A, M: ReplyMode> WasmCtx<'a, A, M> {
    /// ADR-0114 addressing amendment: a sendable handle to this actor's
    /// **parent** in the cluster, or `None` if this actor is the cluster
    /// root. The cluster root's lineage parent is outside the module (the
    /// component host, for a loaded component), and no ctx proves it
    /// (ADR-0230 §3). Reach the actor that loaded this one through the sender
    /// of its mail ([`WasmCtx::sender`]) or through an `ErasedActorPath` in config
    /// ([`WasmCtx::resolve_path`]).
    ///
    /// Resolves by registry lookup over the per-component inline registry,
    /// never by folding (a [`MailboxId`] is a one-way hash chain, so the
    /// guest cannot reproduce the parent id; it looks the recorded parent
    /// up). A send through the returned handle routes in place through the
    /// cluster membrane.
    #[must_use]
    pub fn parent(&self) -> Option<RelativeMailbox<'a>> {
        let id = self.inline.parent_of(MailboxId(self.mailbox))?;
        Some(RelativeMailbox { id, sender: self.mailbox, inline: self.inline })
    }

    /// ADR-0114 addressing amendment: a sendable handle to this actor's
    /// inline **child** whose subname is `name`, or `None` if no such child
    /// is resident in the cluster. Pure registry lookup, never a fold.
    #[must_use]
    pub fn child(&self, name: &str) -> Option<RelativeMailbox<'a>> {
        let id = self.inline.child_of(MailboxId(self.mailbox), name)?;
        Some(RelativeMailbox { id, sender: self.mailbox, inline: self.inline })
    }

    /// ADR-0114 addressing amendment: a sendable handle to this actor's
    /// **sibling** whose subname is `name` — the child of this actor's
    /// parent named `name` — or `None` if this actor has no recorded parent
    /// or no such sibling resides. Pure registry lookup, never a fold.
    #[must_use]
    pub fn sibling(&self, name: &str) -> Option<RelativeMailbox<'a>> {
        let id = self.inline.sibling_of(MailboxId(self.mailbox), name)?;
        Some(RelativeMailbox { id, sender: self.mailbox, inline: self.inline })
    }
}
