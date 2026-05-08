//! [`MailSender`] — outbound-mail surface every actor ctx exposes.
//!
//! Per-stage capability trait under the issue 663 refactor (target shape
//! of `aether-actor::actor::ctx`). Both init-time and runtime ctxs across
//! every transport implement [`MailSender`]; the per-host concrete ctx
//! struct (today: parametric `Ctx<'a, T>` / `InitCtx<'a, T>` /
//! `DropCtx<'a, T>` in `actor::ctx::parametric`; substrate's
//! `NativeCtx<'a>` / `NativeInitCtx<'a>`) impls it with
//! `type Transport = <its transport>` and the default-impl bodies
//! cover the routing.
//!
//! Phase A of issue 663 adds the trait alongside the existing
//! [`crate::actor::sender::Sender`] / [`crate::actor::sender::MailCtx`]
//! pair. `Sender` continues to back current call sites; the new trait
//! is dead code at this phase. Phase B impls it on the existing ctx
//! types, Phase C concretises the FFI ctx structs and retires the
//! parametric `Ctx<'a, T>`, Phase D switches user-facing actor methods
//! to the generic-bounds API.

use aether_data::{Kind, mailbox_id_from_name};

use crate::actor::{Actor, HandlesKind, Singleton};
use crate::mail::mailbox::{ActorMailbox, resolve_mailbox};
use crate::mail::transport::MailTransport;

/// Outbound-mail surface every actor ctx exposes. The associated
/// `Transport` plumbs the per-host send path so default-impl bodies
/// cover both the FFI and native paths in one place.
///
/// `R: Actor + HandlesKind<K>` is the compile-time gate: trying to
/// send a kind the receiver doesn't handle is rejected at the call
/// site, not silently warn-dropped at runtime. Wire shape (cast or
/// postcard) follows `Kind::encode_into_bytes` (issue #240).
pub trait MailSender {
    /// The transport this ctx routes outbound mail through. FFI ctxs
    /// pin this to the FFI ZST transport; native ctxs pin to
    /// `NativeTransport`.
    type Transport: MailTransport;

    /// Borrow the actor's transport. Default-impl bodies for `send` /
    /// `send_many` / `send_to_named` route through this.
    fn transport(&self) -> &Self::Transport;

    /// Singleton sender shortcut: returns a typed [`ActorMailbox`]
    /// addressing the unique instance of receiver actor `R`.
    fn actor<R: Singleton>(&self) -> ActorMailbox<'_, R, Self::Transport> {
        ActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport())
    }

    /// Multi-instance sender: resolve a typed [`ActorMailbox`] from a
    /// runtime instance name.
    fn resolve_actor<R: Actor>(&self, name: &str) -> ActorMailbox<'_, R, Self::Transport> {
        ActorMailbox::__new(mailbox_id_from_name(name).0, self.transport())
    }

    /// Send a single payload of kind `K` to the singleton instance of
    /// receiver actor `R`.
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        ActorMailbox::<R, Self::Transport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport(),
        )
        .send(payload);
    }

    /// Send a slice of cast-shape payloads as a contiguous batch.
    /// Cast-only — postcard has no efficient batched wire shape.
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        ActorMailbox::<R, Self::Transport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport(),
        )
        .send_many(payloads);
    }

    /// String-keyed escape hatch for callers that genuinely don't
    /// know the receiver type at compile site (debug tools, dynamic
    /// dispatch, components addressing user-named mailboxes the
    /// substrate registered without a corresponding Rust type).
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        resolve_mailbox::<K, Self::Transport>(name).send(self.transport(), payload);
    }
}
