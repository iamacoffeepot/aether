//! Who this ctx is, and who it can address.
//!
//! The receiver-addressing methods are emitted from one macro because
//! [`NativeCtx`] and [`NativeInitCtx`](super::NativeInitCtx) hold the same
//! binding and must resolve identically (ADR-0099 §5). Beside them sits the
//! outbound lineage the returned handles capture: a `send` from a handle
//! inherits the handler's causal chain (ADR-0080 §7), so the pair that
//! derives it belongs with the pair that reads it.

use aether_actor::{
    ActorRef, Addressable, CallerAddressable, CallerScope, CallerScoped, Instanced, ReplyMode, Singleton,
    address_candidate,
};
use aether_data::{Address, MailId, MailboxId};

use crate::actor::native::mailbox::NativeActorMailbox;

use super::NativeCtx;

/// The receiver-addressing methods shared verbatim by [`NativeCtx`] and
/// [`NativeInitCtx`](super::NativeInitCtx): both hold the same `binding`, so `actor` /
/// `resolve_actor` / `actor_at` resolve identically. Emitting them from
/// one source keeps the two ctxs from drifting and means the bodies are
/// not a `DuplicatedCode` clone (ADR-0099 §5 / issue 1431).
macro_rules! native_sender_methods {
    () => {
        /// Singleton sender shortcut: returns a typed [`NativeActorMailbox`]
        /// addressing the unique instance of receiver actor `R`. The
        /// handle captures this ctx's in-flight lineage (ADR-0080 §7) so
        /// a `send` from it inherits the handler's causal chain.
        #[must_use]
        pub fn actor<R: Singleton + CallerAddressable>(&self) -> NativeActorMailbox<'_, R> {
            let (parent, root) = self.outbound_lineage();
            NativeActorMailbox::__new_in_flight(
                R::resolve(self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE).0, ()).0,
                self.binding,
                parent,
                root,
            )
        }

        /// Multi-instance sender: resolve a typed [`NativeActorMailbox`] from
        /// a runtime instance key through `R`'s caller-scoped resolver.
        /// Captures the in-flight lineage like [`Self::actor`].
        #[must_use]
        pub fn resolve_actor<R: Instanced + CallerAddressable>(&self, name: &str) -> NativeActorMailbox<'_, R> {
            let (parent, root) = self.outbound_lineage();
            NativeActorMailbox::__new_in_flight(
                R::resolve(self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE).0, name).0,
                self.binding,
                parent,
                root,
            )
        }

        /// Address an actor by a [`MailboxId`] already in hand — the id a
        /// `spawn_child` returned, or one a peer handed over. ADR-0099 §3:
        /// a hosted / nested actor's id is the lineage fold, not
        /// `hash(name)`, so it cannot be re-derived from a name; a supervisor
        /// that tracks its children's ids addresses them through this rather
        /// than re-resolving by name. Captures the in-flight lineage like
        /// [`Self::actor`]. For a proven [`ActorRef`], use [`Self::to`] instead.
        #[must_use]
        pub fn actor_at<R: Addressable>(&self, id: MailboxId) -> NativeActorMailbox<'_, R> {
            let (parent, root) = self.outbound_lineage();
            NativeActorMailbox::__new_in_flight(id.0, self.binding, parent, root)
        }

        /// Send through a proven [`ActorRef`]: returns a typed [`NativeActorMailbox`]
        /// addressing the reference's id. Captures the in-flight lineage like
        /// [`Self::actor`].
        #[must_use]
        pub fn to<R: Addressable>(&self, target: &ActorRef<R>) -> NativeActorMailbox<'_, R> {
            let (parent, root) = self.outbound_lineage();
            NativeActorMailbox::__new_in_flight(target.id().0, self.binding, parent, root)
        }

        /// Resolve `address` to a proven [`ActorRef`]: `Some` only when the
        /// registry holds a `Live` route at the position the address names,
        /// `None` for `Starting`, `Dropped`, and `Unknown` alike (ADR-0230).
        /// The one fallible conversion from a description to a proof, and
        /// the only way a reference received from anywhere becomes usable.
        /// Sends no mail: a registry read of the published route view.
        #[must_use]
        pub fn resolve<R: CallerAddressable>(&self, address: &Address<R>) -> Option<ActorRef<R>> {
            let candidate = address_candidate(
                address,
                self.binding.self_mailbox(),
                self.binding.scope_mailbox(CallerScope::Parent),
            )?;
            self.binding.mailer().registry().proven(candidate)
        }
    };
}

pub(super) use native_sender_methods;

impl<M: ReplyMode, A> NativeCtx<'_, M, A> {
    /// The actor's own [`MailboxId`] — the handler-ctx mirror of
    /// [`NativeInitCtx::self_id`](super::NativeInitCtx::self_id). A cap that subscribes settlement or
    /// keys a per-instance table from inside a handler needs its own
    /// address without caching it from `init`; the deferred-reply route
    /// glue (ADR-0154) is the motivating consumer.
    #[must_use]
    pub fn self_id(&self) -> MailboxId {
        self.binding.self_mailbox()
    }

    native_sender_methods!();

    /// ADR-0080 §5: derive the `parent_mail` to stamp on outbound
    /// mail from this ctx's in-flight context. `MailId::NONE` collapses
    /// to `None` (chassis-root or close/init ctx).
    pub(crate) fn outbound_parent(&self) -> Option<MailId> {
        if self.in_flight_mail_id == MailId::NONE {
            None
        } else {
            Some(self.in_flight_mail_id)
        }
    }

    /// ADR-0080 §5: derive the inherited `root` to stamp on outbound
    /// mail from this ctx's in-flight context. `MailId::NONE` collapses
    /// to `None`, in which case `NativeBinding::send_mail_with_lineage`
    /// mints a fresh root from the outbound's own `mail_id`.
    pub(crate) fn outbound_root(&self) -> Option<MailId> {
        if self.in_flight_root == MailId::NONE {
            None
        } else {
            Some(self.in_flight_root)
        }
    }

    /// The `(parent, root)` pair a [`NativeActorMailbox`] captures at
    /// construction so a `send` from the handle inherits this handler's
    /// causal chain (ADR-0080 §7). The `native_sender_methods!` macro
    /// reads it; [`NativeInitCtx`](super::NativeInitCtx) provides a `(None, None)` counterpart
    /// (init has no inbound chain to inherit).
    fn outbound_lineage(&self) -> (Option<MailId>, Option<MailId>) {
        (self.outbound_parent(), self.outbound_root())
    }
}
