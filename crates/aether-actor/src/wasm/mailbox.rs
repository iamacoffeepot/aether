// Wire-encode: `usize → u32` narrowings forward batch lengths to the
// wasm32 host-fn ABI (`_p32` convention, ADR-0024).
#![allow(clippy::cast_possible_truncation)]

//! [`WasmActorMailbox`] and [`WasmActorMailboxWithContext`] — actor-typed
//! sender handles for FFI guests.
//!
//! Issue 665 split the prior parametric `ActorMailbox<'a, R, T>` into
//! per-side types so the `MailTransport` trait can retire. Issue 1987
//! made the FFI variant a ctx-bound transient (`WasmActorMailbox<'a, R>`),
//! symmetric with the native `NativeActorMailbox<'a, R>`: it carries the
//! resolving actor's own id as the send's "from" half plus a borrow of
//! the per-component inline registry the send routes through. The `'a`
//! borrow keeps origin a property of the executing actor — the handle
//! cannot be stored past the handler, so it can never carry a stale
//! origin.
//!
//! Built via [`crate::wasm::ctx::WasmCtx::actor`] /
//! [`crate::wasm::ctx::WasmCtx::resolve_actor`]. The compile-time
//! `R: HandlesKind<K>` gate is the same as the prior parametric form:
//! `ctx.actor::<RenderCapability>().send(&triangle)` compiles only when
//! `RenderCapability: HandlesKind<DrawTriangle>`.

use core::marker::PhantomData;

use aether_data::{Kind, MailboxId, RequestId, Source};

use crate::model::{Addressable, ChildOf, HandlesKind, Instanced};
use crate::wasm::bridge::mail;
use crate::wasm::inline::{ChainMode, Registry, RouteDecision};

/// Phantom-typed receiver-actor handle for FFI guests, built by
/// [`crate::wasm::WasmCtx::actor`] / [`crate::wasm::WasmCtx::resolve_actor`].
///
/// Issue 1987 made it a ctx-bound transient (mirroring the native
/// `NativeActorMailbox<'a, R>` and the in-cluster [`crate::wasm::RelativeMailbox`]):
/// it carries the resolving actor's own folded id as the `sender` (the "from"
/// half every send stamps as origin) plus a borrow of the per-component inline
/// registry the send routes through. The `'a` borrow is what keeps origin a
/// property of the *executing* actor — the handle cannot outlive the handler,
/// so it can never carry a stale origin the way a stored address-only token
/// would.
// Dropping `Mailbox` yields `WasmActor`, colliding with the trait of the same name.
#[allow(clippy::module_name_repetitions)]
pub struct WasmActorMailbox<'a, R> {
    mailbox: u64,
    /// The resolving actor's own folded [`MailboxId`] raw value —
    /// the "from" half threaded onto every send so the recipient's
    /// `ctx.source_mailbox()` resolves who sent it, and so the host stamps the
    /// correct origin without an ambient per-receive cell (issue 1987). Set by
    /// the ctx-level constructors to the resolving ctx's own id.
    sender: u64,
    /// The per-component inline registry the send routes through
    /// ([`Registry::route_or_enqueue`]): a cluster-member recipient
    /// dispatches in place, any other recipient hands off to the host. A typed
    /// peer / cap recipient is always cross-cluster, so this resolves to the
    /// host send — the registry borrow only keeps the routing path uniform with
    /// the in-cluster [`crate::wasm::RelativeMailbox`].
    inline: &'a Registry,
    _r: PhantomData<fn() -> R>,
}

impl<R> Copy for WasmActorMailbox<'_, R> {}
impl<R> Clone for WasmActorMailbox<'_, R> {
    fn clone(&self) -> Self {
        *self
    }
}

/// A [`WasmActorMailbox`] with one typed request context bound for subsequent
/// sends.
///
/// Built by [`WasmActorMailbox::with_context`]. The underlying mailbox is
/// copied into the adapter while `context` stays borrowed, so callers can use
/// capability facades without rebuilding the context at every send.
#[allow(clippy::module_name_repetitions)]
pub struct WasmActorMailboxWithContext<'mailbox, 'context, R, C: Kind> {
    mailbox: WasmActorMailbox<'mailbox, R>,
    context: &'context C,
}

impl<R, C: Kind> Copy for WasmActorMailboxWithContext<'_, '_, R, C> {}
impl<R, C: Kind> Clone for WasmActorMailboxWithContext<'_, '_, R, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, R> WasmActorMailbox<'a, R> {
    /// Not part of the public API; the ctx-level constructors go
    /// through here so the fields stay private. `sender` is the
    /// resolving actor's own id (the "from" half); `inline` is the ctx's
    /// per-component inline registry the send routes through.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64, sender: u64, inline: &'a Registry) -> Self {
        Self { mailbox, sender, inline, _r: PhantomData }
    }

    /// The receiver's typed mailbox id. Exposed for callers that need
    /// it for diagnostics or a host fn the SDK doesn't yet wrap.
    #[must_use]
    pub fn mailbox_id(&self) -> MailboxId {
        MailboxId(self.mailbox)
    }

    /// Bind one typed request context to this mailbox.
    ///
    /// The returned adapter's [`WasmActorMailboxWithContext::send`] stores the
    /// context under each send's minted correlation id.
    #[must_use]
    pub fn with_context<'context, C: Kind>(
        &self,
        context: &'context C,
    ) -> WasmActorMailboxWithContext<'a, 'context, R, C> {
        WasmActorMailboxWithContext { mailbox: *self, context }
    }

    /// Rewrap this physical `mailbox` id as another recipient type while
    /// inheriting this handle's ctx binding (`sender` + inline registry), so
    /// the rewrapped handle's sends stamp the same origin and route the same
    /// way. Use this after a typed [`Self::resolve`] chain when the physical
    /// actor hosts a different logical recipient interface.
    #[must_use]
    pub fn at<Peer>(&self, mailbox: u64) -> WasmActorMailbox<'a, Peer> {
        WasmActorMailbox::__new(mailbox, self.sender, self.inline)
    }

    /// Resolve the instanced child actor `Child` named `name` directly
    /// beneath this actor.
    ///
    /// The declared [`ChildOf<R>`] relationship proves the placement is
    /// legal, while `Child`'s resolver owns the address construction. The
    /// returned handle retains this mailbox's sender and inline registry.
    #[must_use]
    pub fn resolve<Child>(&self, name: &str) -> WasmActorMailbox<'a, Child>
    where
        R: Addressable,
        Child: ChildOf<R> + Instanced,
    {
        self.at(Child::resolve(self.mailbox_id().0, name).0)
    }
}

impl<R: Addressable, C: Kind> WasmActorMailboxWithContext<'_, '_, R, C> {
    /// Send a request with the bound context and return the host-minted
    /// correlation id.
    #[must_use]
    pub fn send<K>(&self, payload: &K) -> RequestId
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        self.mailbox.send_with_context(payload, self.context)
    }
}

impl<R: Addressable> WasmActorMailbox<'_, R> {
    /// Send a single payload of kind `K` to actor `R`. Compile-checked
    /// against `R: HandlesKind<K>` — wrong-kind sends are rejected at
    /// the call site.
    ///
    /// Threads the resolving actor's own id as the send's `from`
    /// (issue 1987): the host stamps it as origin (validated in-cluster),
    /// so the recipient's `ctx.source_mailbox()` resolves the sender with
    /// no ambient host cell. Inherits the handler's in-flight causal
    /// chain by default (ADR-0080 §7): the host stamps the dispatch's
    /// `parent`/`root` onto this send, so the recipient's work settles
    /// back into the caller's chain. Reach for [`Self::send_detached`]
    /// for the rare fire-and-forget send that should start its own chain.
    ///
    /// Wire shape (cast or structured) follows `Kind::encode_into_bytes`
    /// — same single source of truth as the kind-typed sends per
    /// issue #240.
    pub fn send<K>(&self, payload: &K)
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(self.mailbox, K::ID.0, &bytes, 1, ChainMode::Inherit, self.sender);
    }

    /// Send a request and return the correlation id the host minted for it.
    ///
    /// Inline-cluster local sends never leave the guest, so no host correlation
    /// exists for them. That path warn-logs and returns the no-correlation
    /// sentinel rather than reading a stale `prev_correlation_p32` value.
    #[must_use]
    pub fn send_tracked<K>(&self, payload: &K) -> RequestId
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        match self.inline.route_decision(self.mailbox) {
            RouteDecision::Local => {
                self.send(payload);
                tracing::warn!(
                    kind = <K as Kind>::NAME,
                    recipient = self.mailbox,
                    "send_tracked on an inline-cluster local route has no host correlation",
                );
                RequestId(Source::NO_CORRELATION)
            }
            RouteDecision::Remote => {
                self.send(payload);
                RequestId(mail::prev_correlation())
            }
        }
    }

    /// Send a request and store a typed context under the minted correlation
    /// id for the eventual reply handler to recover with
    /// [`WasmCtx::take_context`](crate::wasm::WasmCtx::take_context).
    #[must_use]
    pub fn send_with_context<K, C>(&self, payload: &K, context: &C) -> RequestId
    where
        R: HandlesKind<K>,
        K: Kind,
        C: Kind,
    {
        let request = self.send_tracked(payload);
        if request.0 != Source::NO_CORRELATION {
            // SAFETY: the macro-emitted registry is accessed only under the
            // serialized wasm guest entrypoint.
            unsafe {
                self.inline.request_contexts_mut().insert(request, context);
            }
        }
        request
    }

    /// Send a slice of payloads as a contiguous batch. Cast-only —
    /// see [`crate::model::ctx::MailSender::send_many`] for the
    /// wire-shape rationale. Inherits the handler's causal chain like
    /// [`Self::send`].
    pub fn send_many<K>(&self, payloads: &[K])
    where
        R: HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.inline.route_or_enqueue(
            self.mailbox,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            ChainMode::Inherit,
            self.sender,
        );
    }

    /// ADR-0080 §7 fire-and-forget escape hatch: send `payload` to `R`
    /// without inheriting the handler's in-flight causal chain. The
    /// host mints a fresh root, so the recipient processes the mail as
    /// the start of a new tree.
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
        self.inline.route_or_enqueue(self.mailbox, K::ID.0, &bytes, 1, ChainMode::Detached, self.sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr;

    use crate::model::{Many, One};

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
        let inline = Registry::new();
        let parent = WasmActorMailbox::<Parent>::__new(0xCA11_AB1E, 0x5EED, &inline);

        let child = parent.resolve::<Child>("camera");

        assert_eq!(child.mailbox_id(), Child::resolve(parent.mailbox_id().0, "camera"));
        assert_eq!(child.sender, parent.sender);
        assert!(ptr::eq(child.inline, parent.inline));
    }
}
