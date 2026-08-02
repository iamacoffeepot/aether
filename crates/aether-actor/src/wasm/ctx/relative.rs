//! Cluster-relative addressing — [`RelativeMailbox`] and the
//! [`WasmCtx::parent`] / [`WasmCtx::child`] / [`WasmCtx::sibling`] verbs
//! that resolve one (ADR-0114 addressing amendment).

use aether_data::{Kind, MailboxId};

use super::WasmCtx;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::wasm::inline::{ChainMode, Registry};

/// A type-erased sendable handle to a cluster relative — the parent,
/// a sibling, or a child of the addressing actor (ADR-0114 addressing
/// amendment). Returned by [`WasmCtx::parent`] / [`WasmCtx::sibling`] /
/// [`WasmCtx::child`], it wraps the relative's resolved [`MailboxId`] (looked
/// up in the per-component inline registry, never folded) plus the registry
/// the send routes through.
///
/// Unlike [`WasmActorMailbox`](crate::WasmActorMailbox) this carries no receiver type and no
/// `R: HandlesKind<K>` bound — relative addressing is positional, so the
/// target's handler set is not known at the call site (the by-id counterpart
/// of the runtime-name `send_to_named` escape hatch). The send routes through
/// the inline registry's cluster router: a cluster-member recipient (which a
/// resolved relative always is) dispatches in place via the queue + drain,
/// never the scheduler.
pub struct RelativeMailbox<'a> {
    id: MailboxId,
    /// The addressing actor's own folded [`MailboxId`] raw value — the "from"
    /// half stamped on the in-place send so the relative recipient's
    /// `ctx.source_mailbox()` resolves who sent it. Set by
    /// [`WasmCtx::parent`] / [`WasmCtx::child`] / [`WasmCtx::sibling`] to the
    /// resolving ctx's `mailbox`.
    sender: u64,
    inline: &'a Registry,
}

impl RelativeMailbox<'_> {
    /// The relative's resolved [`MailboxId`].
    #[must_use]
    pub fn mailbox_id(&self) -> MailboxId {
        self.id
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
    pub fn send<K: Kind>(&self, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(self.id.0, K::ID.0, &bytes, 1, ChainMode::Inherit, self.sender);
    }

    /// Forward pre-encoded `bytes` of kind `kind` to this relative — the
    /// type-erased counterpart of [`Self::send`], for a mail-forwarding
    /// interposer (the ADR-0137 behavior host, issue 2687) that reroutes an
    /// arbitrary inbound kind it holds no Rust type for. Routes the same way
    /// [`Self::send`] does — in place through the cluster membrane, inheriting
    /// the handler's causal chain — so the interposer stays transparent to
    /// settlement. `count` is fixed at 1: a forward carries one inbound mail.
    pub fn send_bytes(&self, kind: aether_data::KindId, bytes: &[u8]) {
        self.inline.route_or_enqueue(self.id.0, kind.0, bytes, 1, ChainMode::Inherit, self.sender);
    }

    /// Fire-and-forget send to this relative (ADR-0080 §7 detach signal).
    /// In-cluster the recipient dispatches in place regardless; the detach
    /// flag rides through only on the cross-cluster fallback path, which a
    /// resolved relative never takes.
    pub fn send_detached<K: Kind>(&self, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(self.id.0, K::ID.0, &bytes, 1, ChainMode::Detached, self.sender);
    }
}

impl<'a, M: ReplyMode> WasmCtx<'a, M> {
    /// ADR-0114 addressing amendment: a sendable handle to this actor's
    /// **parent** in the cluster, or `None` if this actor is the cluster
    /// root (the instance itself — its parent is cross-cluster, addressed
    /// through a chassis cap or the runtime-name escape hatch, not here).
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
