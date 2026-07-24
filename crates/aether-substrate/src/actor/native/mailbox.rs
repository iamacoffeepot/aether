//! [`NativeActorMailbox`] and [`NativeActorMailboxWithContext`] — actor-typed
//! sender handles for native ctxs.
//!
//! Issue 665 split the prior parametric `aether_actor::ActorMailbox<'a, R, T>`
//! into per-side types so the `MailTransport` trait can retire. The
//! native variant borrows the actor's [`NativeBinding`] reference
//! (via the `'a` lifetime) and dispatches through the inherent
//! `NativeBinding::send_mail` — no trait-method round-trip, no
//! FFI-shaped wrapper.
//!
//! Built via [`NativeCtx::actor`](crate::actor::native::ctx::NativeCtx) /
//! [`NativeCtx::resolve_actor`](crate::actor::native::ctx::NativeCtx) and
//! their init variants.
//! The compile-time `R: HandlesKind<K>` gate is the same as the prior
//! parametric form: `ctx.actor::<RenderCapability>().send(&triangle)`
//! compiles only when `RenderCapability: HandlesKind<DrawTriangle>`.

use core::marker::PhantomData;

use aether_actor::{Addressable, ChildOf, HandlesKind, Instanced};
use aether_data::{ActorId, Kind, MailId, RequestId, Tag, fold_lineage, with_tag};

use crate::actor::native::binding::NativeBinding;

/// Phantom-typed receiver-actor handle for native callers. Carries a
/// borrow of the sender's [`NativeBinding`] so `send` /
/// `send_many` are `&self`-receiver and don't require threading a
/// binding reference at every call site.
///
/// Multi-kind by construction: `send::<K>` is gated on
/// `R: HandlesKind<K>`, so the same
/// `NativeActorMailbox<'_, RenderCapability>` accepts both
/// `&DrawTriangle` and `&ViewProjection`. Wrong-kind sends are compile errors.
pub struct NativeActorMailbox<'a, R> {
    mailbox: u64,
    binding: &'a NativeBinding,
    /// ADR-0080 §7: the in-flight handler lineage captured at
    /// construction (`ctx.actor::<R>()` time), so a `send` from the
    /// handle inherits the caller's causal chain without re-threading
    /// the ctx. `None`/`None` is the chassis-root / no-inbound shape —
    /// a fresh chain — which is also what [`Self::__new`] (the detached
    /// base constructor) and [`Self::send_detached`] produce.
    parent: Option<MailId>,
    root: Option<MailId>,
    _r: PhantomData<fn() -> R>,
}

impl<R> Copy for NativeActorMailbox<'_, R> {}
impl<R> Clone for NativeActorMailbox<'_, R> {
    fn clone(&self) -> Self {
        *self
    }
}

/// A [`NativeActorMailbox`] with one typed request context bound for
/// subsequent sends.
///
/// Built by [`NativeActorMailbox::with_context`]. The underlying mailbox is
/// copied into the adapter while `context` stays borrowed, so callers can use
/// capability facades without rebuilding the context at every send.
pub struct NativeActorMailboxWithContext<'mailbox, 'context, R, C: Kind> {
    mailbox: NativeActorMailbox<'mailbox, R>,
    context: &'context C,
}

impl<R, C: Kind> Copy for NativeActorMailboxWithContext<'_, '_, R, C> {}
impl<R, C: Kind> Clone for NativeActorMailboxWithContext<'_, '_, R, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, R> NativeActorMailbox<'a, R> {
    /// Not part of the public API; external cap-owned ext facades that
    /// hold only a binding (no in-flight ctx) build a **detached**
    /// handle through here — `send` from it mints a fresh causal chain.
    /// The per-handler ctx constructors go through
    /// [`Self::__new_in_flight`] instead so the everyday
    /// `ctx.actor::<R>().send()` inherits the handler's chain.
    #[doc(hidden)]
    pub fn __new(mailbox: u64, binding: &'a NativeBinding) -> Self {
        Self { mailbox, binding, parent: None, root: None, _r: PhantomData }
    }

    /// Not part of the public API; the per-handler
    /// [`NativeCtx`](crate::actor::native::ctx::NativeCtx)
    /// constructors (`actor` / `resolve_actor` / `actor_at`) go through
    /// here, capturing the handler's in-flight `parent` / `root` so a
    /// `send` from the returned handle inherits the caller's causal
    /// chain (ADR-0080 §7). `None`/`None` collapses to the same fresh-
    /// chain shape as [`Self::__new`].
    #[doc(hidden)]
    pub fn __new_in_flight(
        mailbox: u64,
        binding: &'a NativeBinding,
        parent: Option<MailId>,
        root: Option<MailId>,
    ) -> Self {
        Self { mailbox, binding, parent, root, _r: PhantomData }
    }

    /// The receiver's typed mailbox id. Exposed for callers that need
    /// it for diagnostics or a host fn the SDK doesn't yet wrap.
    #[must_use]
    pub fn mailbox_id(&self) -> aether_data::MailboxId {
        aether_data::MailboxId(self.mailbox)
    }

    /// Bind one typed request context to this mailbox.
    ///
    /// The returned adapter's [`NativeActorMailboxWithContext::send`] stores
    /// the context under each send's minted correlation id.
    #[must_use]
    pub fn with_context<'context, C: Kind>(
        &self,
        context: &'context C,
    ) -> NativeActorMailboxWithContext<'a, 'context, R, C> {
        NativeActorMailboxWithContext { mailbox: *self, context }
    }

    /// The transport binding this handle dispatches through. Not part of
    /// the public API; a cap-owned ext facade that composes a
    /// non-trivial id (e.g. a multi-step lineage fold for a grandchild)
    /// rewraps it onto the same binding via [`Self::__new`].
    #[doc(hidden)]
    #[must_use]
    pub fn binding(&self) -> &'a NativeBinding {
        self.binding
    }

    /// Rewrap a precomputed `mailbox` id as another recipient type while
    /// retaining this handle's binding and in-flight causal context.
    #[must_use]
    pub fn at<Recipient>(&self, mailbox: u64) -> NativeActorMailbox<'a, Recipient> {
        NativeActorMailbox::__new_in_flight(mailbox, self.binding, self.parent, self.root)
    }

    /// Resolve the instanced child actor `Child` named `name` directly
    /// beneath this actor.
    ///
    /// The declared [`ChildOf<R>`] relationship proves the placement is
    /// legal, while `Child`'s resolver owns the address construction. The
    /// returned handle retains this mailbox's binding and in-flight causal
    /// context.
    #[must_use]
    pub fn resolve<Child>(&self, name: &str) -> NativeActorMailbox<'a, Child>
    where
        R: Addressable,
        Child: ChildOf<R> + Instanced,
    {
        self.at(Child::resolve(self.mailbox_id().0, name).0)
    }

    /// Resolve a child mailbox of *this* actor, where the child is the
    /// instanced node `scope:segment` (ADR-0099 §3). The child's id folds
    /// that node's `ActorId` onto this actor's lineage carry, so a cap
    /// that hosts children — the component host reaching a loaded
    /// component, a socket listener reaching a session — composes the
    /// registered fold id without allocating a name. `self.mailbox` is
    /// the parent carry (exact for a root-pinned cap, depth-1). Threads
    /// the existing `'a` binding ref from the parent handle.
    #[must_use]
    pub fn resolve_peer_scoped<Peer: Addressable>(&self, scope: &str, segment: &str) -> NativeActorMailbox<'a, Peer> {
        let node = ActorId::instanced(scope, segment);
        NativeActorMailbox::__new_in_flight(
            with_tag(Tag::Mailbox, fold_lineage(self.mailbox, node)),
            self.binding,
            self.parent,
            self.root,
        )
    }
}

impl<R: Addressable, C: Kind> NativeActorMailboxWithContext<'_, '_, R, C> {
    /// Send a request with the bound context and return the minted mail id.
    #[must_use]
    pub fn send<K>(&self, payload: &K) -> MailId
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        self.mailbox.send_with_context(payload, self.context)
    }
}

impl<R: Addressable> NativeActorMailbox<'_, R> {
    /// Send a single payload of kind `K` to actor `R`. Compile-checked
    /// against `R: HandlesKind<K>`. Wire shape (cast or structured)
    /// follows `Kind::encode_into_bytes`.
    ///
    /// Inherits the handler's in-flight causal chain by default
    /// (ADR-0080 §7): the lineage captured at `ctx.actor::<R>()` time
    /// rides onto the mail, so the recipient's work settles back into
    /// the caller's chain and an outbound send arms a settlement
    /// obligation rather than truncating the trace at the send. Reach
    /// for [`Self::send_detached`] for the rare fire-and-forget send
    /// that should start its own chain.
    pub fn send<K>(&self, payload: &K)
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        // 2b: buffer into the actor's send-side ring with the captured
        // in-flight lineage. Flushed at handler end by `NativeCtx`'s `Drop`.
        let _ = self.binding.push_envelope_buffered(self.mailbox, K::ID.0, &bytes, 1, self.parent, self.root);
    }

    /// Send a slice of payloads as a contiguous batch. Cast-only.
    /// Inherits the handler's causal chain like [`Self::send`].
    pub fn send_many<K>(&self, payloads: &[K])
    where
        R: HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        // Batch count rides as `u32` on the wire (matches the FFI ABI);
        // realistic mail batches stay well below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let count = payloads.len() as u32;
        let _ = self.binding.push_envelope_buffered(self.mailbox, K::ID.0, bytes, count, self.parent, self.root);
    }

    /// ADR-0080 §7 fire-and-forget escape hatch: send `payload` to `R`
    /// without inheriting the handler's in-flight causal chain. The
    /// recipient processes the mail as the root of a new tree.
    ///
    /// **Fire-and-forget only.** A detached send mints no parent
    /// linkage, so any reply the recipient issues inherits the
    /// *recipient's* tree rather than the sender's. Reply-correlated
    /// requests always go through [`Self::send`].
    pub fn send_detached<K>(&self, payload: &K)
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        let _ = self.binding.push_envelope_buffered(self.mailbox, K::ID.0, &bytes, 1, None, None);
    }

    /// Like [`Self::send_detached`] but returns the minted `MailId` — the
    /// fresh chain's root — so the caller can correlate the recipient's
    /// eventual reply (ADR-0042 echoes the correlation id) and subscribe to
    /// the chain's settlement. The typed form of the deferral pattern a
    /// request/reply-shaped handler uses when it must not extend its own
    /// inbound chain (e.g. an HTTP router holding the reply obligation across
    /// the async boundary): the downstream dispatch roots a new tree, and the
    /// returned id keys the held reply guard. The compile-time
    /// `R: HandlesKind<K>` gate is the same as [`Self::send`].
    #[must_use]
    pub fn send_detached_tracked<K>(&self, payload: &K) -> MailId
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_buffered(self.mailbox, K::ID.0, &bytes, 1, None, None)
    }

    /// Like [`Self::send`] but returns the minted `MailId` so the caller
    /// can subscribe to its settlement via the chassis
    /// [`crate::chassis::settlement::SettlementRegistry`]. Inherits the
    /// handler's causal chain the same way `send` does — the only
    /// difference is the returned id.
    ///
    /// Uses this mailbox's stored per-instance id, so settlement
    /// subscription works uniformly for singleton actors
    /// (`ctx.actor::<R>()`) and instanced actors like wasm trampolines
    /// (`ctx.resolve_actor::<R>(name)`). The compile-time
    /// `R: HandlesKind<K>` gate is the same as [`Self::send`].
    ///
    /// When the handle was built at a chassis-root edge (captured
    /// lineage is `None`/`None`), the returned id is itself the root of
    /// a fresh causal chain. When built mid-handler, the returned id is
    /// the new mail's id inside the inherited root chain — subscribing
    /// to it would only fire on settlement of *that mail's* descendants,
    /// not the whole chain. Callers that want chain-root settlement
    /// should build the handle at chassis-root (typical for
    /// capability-init / external-event entry points).
    pub fn send_tracked<K>(&self, payload: &K) -> MailId
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_buffered(self.mailbox, K::ID.0, &bytes, 1, self.parent, self.root)
    }

    /// Send a request and store a typed context under the minted correlation
    /// id for the eventual reply handler to recover with
    /// [`NativeCtx::take_context`](crate::actor::native::ctx::NativeCtx::take_context).
    #[must_use]
    pub fn send_with_context<K, C>(&self, payload: &K, context: &C) -> MailId
    where
        R: HandlesKind<K>,
        K: Kind,
        C: Kind,
    {
        let mail_id = self.send_tracked(payload);
        self.binding.store_request_context(RequestId(mail_id.correlation_id), context);
        mail_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_actor::{Many, One};
    use aether_data::MailboxId;
    use core::ptr;

    use crate::testing::bare_substrate;

    struct Parent;

    impl Addressable for Parent {
        const NAMESPACE: &'static str = "test.parent";
        type Resolver = One;
    }

    struct Child;

    impl Addressable for Child {
        const NAMESPACE: &'static str = "test.child";
        type Resolver = Many;
    }

    impl ChildOf<Parent> for Child {}

    #[test]
    fn resolve_uses_the_child_resolver_and_retains_context() {
        let (_, mailer) = bare_substrate();
        let binding = NativeBinding::new_for_test(mailer, MailboxId(0xCA11_AB1E));
        let parent_mail = MailId::new(MailboxId(0x5EED), 7);
        let root_mail = MailId::new(MailboxId(0x600D), 3);
        let parent =
            NativeActorMailbox::<Parent>::__new_in_flight(0xCA11_AB1E, &binding, Some(parent_mail), Some(root_mail));

        let child = parent.resolve::<Child>("camera");

        assert_eq!(child.mailbox_id(), Child::resolve(parent.mailbox_id().0, "camera"));
        assert!(ptr::eq(child.binding, parent.binding));
        assert_eq!(child.parent, parent.parent);
        assert_eq!(child.root, parent.root);
    }
}
