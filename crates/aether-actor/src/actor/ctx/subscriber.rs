//! [`Subscriber`] — input-stream subscription surface, blanket-impl'd
//! for any ctx that has both [`Resolver`] (to know its own mailbox id)
//! and [`MailSender`] (to dispatch the subscribe mail).
//!
//! Issue 703 split this off [`Resolver`]. Pre-703 `Resolver` carried
//! a `subscribe_input` method and a `MailSender` supertrait, conflating
//! "address lookup" with "mail-sending" and making `subscribe_input`
//! reachable from init contexts. ADR-0079's init stage is the sync
//! constructor; subscribing belongs in `wire`. The two-trait split
//! lets the type system enforce that — only ctxs that genuinely impl
//! both `Resolver` and `MailSender` (i.e. wire / runtime ctxs) pick
//! up `Subscriber`'s blanket impl.
//!
//! [`Resolver`]: crate::actor::ctx::Resolver
//! [`MailSender`]: crate::actor::ctx::MailSender

use aether_data::{Kind, KindId, MailboxId};

use crate::actor::ctx::mail_sender::MailSender;
use crate::actor::ctx::resolver::Resolver;

/// Subscribe to an input stream. The default body mails
/// `aether.input.subscribe { kind: K::ID, mailbox: self.mailbox_id() }`
/// to the input cap; the cap fans out matching events back to this
/// actor's mailbox.
///
/// Blanket impl'd for any `T: Resolver + MailSender` — call sites
/// don't impl this directly, they just bring the trait into scope and
/// `ctx.subscribe_input::<Tick>()` works wherever both supertraits do.
pub trait Subscriber: Resolver + MailSender {
    /// Mail `aether.input.subscribe` for `K`, naming this actor's
    /// mailbox as the subscriber.
    fn subscribe_input<K: Kind + 'static>(&mut self) {
        let payload = aether_kinds::SubscribeInput {
            kind: KindId(K::ID.0),
            mailbox: MailboxId(self.mailbox_id()),
        };
        self.send_to_named("aether.input", &payload);
    }
}

impl<T: Resolver + MailSender> Subscriber for T {}
