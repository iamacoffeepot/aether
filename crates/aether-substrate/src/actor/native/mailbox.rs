//! [`NativeActorMailbox`] — actor-typed sender handle for native ctxs.
//!
//! Issue 665 split the prior parametric `aether_actor::ActorMailbox<'a, R, T>`
//! into per-side types so the `MailTransport` trait can retire. The
//! native variant borrows the actor's [`NativeBinding`] reference
//! (via the `'a` lifetime) and dispatches through the inherent
//! `NativeBinding::send_mail` — no trait-method round-trip, no
//! FFI-shaped wrapper.
//!
//! Built via [`super::ctx::NativeCtx::actor`] /
//! [`super::ctx::NativeCtx::resolve_actor`] and their init variants.
//! The compile-time `R: HandlesKind<K>` gate is the same as the prior
//! parametric form: `ctx.actor::<RenderCapability>().send(&triangle)`
//! compiles only when `RenderCapability: HandlesKind<DrawTriangle>`.

use core::marker::PhantomData;

use aether_actor::{Actor, HandlesKind};
use aether_data::{Kind, MailId, mailbox_id_from_name};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;

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
    _r: PhantomData<fn() -> R>,
}

impl<'a, R> Copy for NativeActorMailbox<'a, R> {}
impl<'a, R> Clone for NativeActorMailbox<'a, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, R> NativeActorMailbox<'a, R> {
    /// Not part of the public API; the ctx-level constructors go
    /// through here so the fields stay private.
    #[doc(hidden)]
    pub fn __new(mailbox: u64, binding: &'a NativeBinding) -> Self {
        Self {
            mailbox,
            binding,
            _r: PhantomData,
        }
    }

    /// The receiver's typed mailbox id. Exposed for callers that need
    /// it for diagnostics or a host fn the SDK doesn't yet wrap.
    pub fn mailbox_id(&self) -> aether_data::MailboxId {
        aether_data::MailboxId(self.mailbox)
    }

    /// Resolve a sibling mailbox on the same binding, addressed by
    /// `name`. Same FNV-hash name resolution as
    /// [`NativeCtx::resolve_actor`] — kept as an inherent method so
    /// cap-owned ext traits (which only have a mailbox in hand, not a
    /// ctx) can hand back peer handles without rethreading the ctx.
    /// Threads the existing `'a` binding ref, so the returned handle
    /// inherits the parent mailbox's borrow lifetime.
    pub fn resolve_peer<Peer: Actor>(&self, name: &str) -> NativeActorMailbox<'a, Peer> {
        NativeActorMailbox::__new(mailbox_id_from_name(name).0, self.binding)
    }
}

impl<'a, R: Actor> NativeActorMailbox<'a, R> {
    /// Send a single payload of kind `K` to actor `R`. Compile-checked
    /// against `R: HandlesKind<K>`. Wire shape (cast or postcard)
    /// follows `Kind::encode_into_bytes`.
    pub fn send<K>(&self, payload: &K)
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.binding.send_mail(self.mailbox, K::ID.0, &bytes, 1);
    }

    /// Send a slice of payloads as a contiguous batch. Cast-only.
    pub fn send_many<K>(&self, payloads: &[K])
    where
        R: HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.binding
            .send_mail(self.mailbox, K::ID.0, bytes, payloads.len() as u32);
    }

    /// ADR-0080: like [`Self::send`] but returns the minted `MailId`
    /// so the caller can subscribe to its settlement via the chassis
    /// [`crate::chassis::settlement::SettlementRegistry`].
    ///
    /// Uses this mailbox's stored per-instance id, so settlement
    /// subscription works uniformly for singleton actors
    /// (`ctx.actor::<R>()`) and instanced actors like wasm trampolines
    /// (`ctx.resolve_actor::<R>(name)`). The compile-time
    /// `R: HandlesKind<K>` gate is the same as [`Self::send`].
    ///
    /// When `ctx` represents a chassis-root edge (in-flight `MailId`
    /// is `NONE`), the returned id is itself the root of a fresh
    /// causal chain. When `ctx` is mid-handler, the returned id is
    /// the new mail's id inside the inherited root chain —
    /// subscribing to it would only fire on settlement of *that
    /// mail's* descendants, not the whole chain. Callers that want
    /// chain-root settlement should be at chassis-root (typical for
    /// capability-init / external-event entry points).
    pub fn send_traced<K>(&self, ctx: &NativeCtx<'_>, payload: &K) -> MailId
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_returning_root(
            self.mailbox,
            K::ID.0,
            &bytes,
            1,
            ctx.outbound_parent(),
            ctx.outbound_root(),
        )
    }
}
