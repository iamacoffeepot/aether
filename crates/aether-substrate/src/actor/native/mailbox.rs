//! [`NativeActorMailbox`] — actor-typed sender handle for native ctxs.
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

use aether_actor::{Addressable, HandlesKind};
use aether_data::{ActorId, Kind, MailId, Tag, fold_lineage, mailbox_id_from_name, with_tag};

use crate::actor::native::binding::NativeBinding;

/// Phantom-typed receiver-actor handle for native callers. Carries a
/// borrow of the sender's [`NativeBinding`] so `send` /
/// `send_many` are `&self`-receiver and don't require threading a
/// binding reference at every call site.
///
/// Multi-kind by construction: `send::<K>` is gated on
/// `R: HandlesKind<K>`, so the same
/// `NativeActorMailbox<'_, RenderCapability>` accepts both
/// `&DrawTriangle` and `&Camera`. Wrong-kind sends are compile errors.
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

impl<'a, R> NativeActorMailbox<'a, R> {
    /// Not part of the public API; external cap-owned ext facades that
    /// hold only a binding (no in-flight ctx) build a **detached**
    /// handle through here — `send` from it mints a fresh causal chain.
    /// The per-handler ctx constructors go through
    /// [`Self::__new_in_flight`] instead so the everyday
    /// `ctx.actor::<R>().send()` inherits the handler's chain.
    #[doc(hidden)]
    pub fn __new(mailbox: u64, binding: &'a NativeBinding) -> Self {
        Self {
            mailbox,
            binding,
            parent: None,
            root: None,
            _r: PhantomData,
        }
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
        Self {
            mailbox,
            binding,
            parent,
            root,
            _r: PhantomData,
        }
    }

    /// The receiver's typed mailbox id. Exposed for callers that need
    /// it for diagnostics or a host fn the SDK doesn't yet wrap.
    #[must_use]
    pub fn mailbox_id(&self) -> aether_data::MailboxId {
        aether_data::MailboxId(self.mailbox)
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

    /// Resolve a sibling mailbox on the same binding, addressed by
    /// `name`. Same FNV-hash name resolution as
    /// [`NativeCtx::resolve_actor`](crate::actor::native::ctx::NativeCtx) —
    /// `name` must be the peer's **full
    /// registered name** (flat ADR-0029 hash). A caller that needs a
    /// lineage-folded child id (ADR-0099 §3) uses
    /// [`Self::resolve_peer_scoped`] instead. Kept as an inherent method
    /// so cap-owned ext traits (which only have a mailbox in hand, not a
    /// ctx) can hand back peer handles without rethreading the ctx.
    /// Threads the existing `'a` binding ref, so the returned handle
    /// inherits the parent mailbox's borrow lifetime.
    // Runtime-name escape hatch (the by-name peer-resolution counterpart of
    // `NativeCtx::resolve_actor`): the peer name is supplied at runtime.
    #[must_use]
    #[allow(clippy::disallowed_methods)]
    pub fn resolve_peer<Peer: Addressable>(&self, name: &str) -> NativeActorMailbox<'a, Peer> {
        // Carry this handle's captured lineage onto the peer handle so a
        // send from it stays in the caller's chain (ADR-0080 §7).
        NativeActorMailbox::__new_in_flight(
            mailbox_id_from_name(name).0,
            self.binding,
            self.parent,
            self.root,
        )
    }

    /// Resolve a child mailbox of *this* actor, where the child is the
    /// instanced node `scope:segment` (ADR-0099 §3). The child's id folds
    /// that node's `ActorId` onto this actor's lineage carry, so a cap
    /// that hosts children — the component host reaching a loaded
    /// component, a socket listener reaching a session — composes the
    /// registered fold id without allocating a name. `self.mailbox` is
    /// the parent carry (exact for a root-pinned cap, depth-1). Threads
    /// the existing `'a` binding ref like [`Self::resolve_peer`].
    #[must_use]
    pub fn resolve_peer_scoped<Peer: Addressable>(
        &self,
        scope: &str,
        segment: &str,
    ) -> NativeActorMailbox<'a, Peer> {
        let node = ActorId::instanced(scope, segment);
        NativeActorMailbox::__new_in_flight(
            with_tag(Tag::Mailbox, fold_lineage(self.mailbox, node)),
            self.binding,
            self.parent,
            self.root,
        )
    }
}

impl<R: Addressable> NativeActorMailbox<'_, R> {
    /// Send a single payload of kind `K` to actor `R`. Compile-checked
    /// against `R: HandlesKind<K>`. Wire shape (cast or postcard)
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
        let _ = self.binding.push_envelope_buffered(
            self.mailbox,
            K::ID.0,
            &bytes,
            1,
            self.parent,
            self.root,
        );
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
        let _ = self.binding.push_envelope_buffered(
            self.mailbox,
            K::ID.0,
            bytes,
            count,
            self.parent,
            self.root,
        );
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
        let _ = self
            .binding
            .push_envelope_buffered(self.mailbox, K::ID.0, &bytes, 1, None, None);
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
        self.binding.push_envelope_buffered(
            self.mailbox,
            K::ID.0,
            &bytes,
            1,
            self.parent,
            self.root,
        )
    }
}
