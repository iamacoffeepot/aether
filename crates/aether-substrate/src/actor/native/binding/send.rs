//! The eager send path — mint a correlation, stamp the lineage, push straight
//! through the mailer — and the correlation counter it advances.

use std::sync::atomic::Ordering;

use super::{NativeBinding, OutboundSend};
use crate::mail::attachments;
use crate::mail::{KindId, Mail, MailId, MailboxId, Source, SourceAddr};

/// Inherent send / `prev_correlation` entry points the
/// per-handler [`super::ctx::NativeCtx`](crate::actor::native::ctx::NativeCtx) / [`super::ctx::NativeInitCtx`](crate::actor::native::ctx::NativeInitCtx)
/// route through. Issue 665 retired the prior `MailTransport` trait
/// impl; the FFI-shaped wrapper served no purpose for native (Mailer
/// dispatch is direct), and `save_state` / `reply_mail` were stubs the
/// trait forced on us. The capability traits in
/// [`aether_actor::model::ctx`] are the only cross-target trait surface
/// post-665.
impl NativeBinding {
    /// Mint an eager envelope's identity, expose it to `before_push`, then
    /// publish the mail. The activation barrier uses this narrow hook to make
    /// its exact identity visible before another owner worker can consume it.
    pub(in crate::actor::native) fn push_envelope_returning_root_before_push(
        &self,
        send: OutboundSend<'_>,
        before_push: impl FnOnce(MailId),
    ) -> MailId {
        let OutboundSend { recipient, kind, bytes, attachments, count, parent_mail, inherited_root } = send;
        let correlation = self.correlation.fetch_add(1, Ordering::AcqRel) + 1;
        let recipient_id = MailboxId(recipient);
        let reply_to = Source::with_correlation(SourceAddr::Component(self.self_mailbox()), correlation);
        let mail_id = MailId::new(self.self_mailbox(), correlation);
        let root = inherited_root.unwrap_or(mail_id);
        before_push(mail_id);
        // ADR-0080 §2 producer hook: emit `Sent` before pushing the
        // mail. Every `Mailer` carries a trace handle by default
        // (per-chassis post iamacoffeepot/aether#953), so producer
        // calls are unconditional; the drainer is the optional piece.
        self.mailer.record_sent(mail_id, root, parent_mail, self.self_mailbox(), recipient_id, KindId(kind));
        let mail = Mail::new(recipient_id, KindId(kind), bytes.to_vec(), count)
            .with_reply_to(reply_to)
            .with_lineage(Some(mail_id), Some(root), parent_mail)
            .with_attachments(attachments::owned(attachments));
        self.mailer.push(mail);
        mail_id
    }

    /// Correlation id the substrate minted for this actor's most
    /// recent send (ADR-0042). `0` before any send. Universal
    /// — every send mints a correlation; a handler stashes it and
    /// matches it against the inbound reply's correlation to pair a
    /// reply with the request it sent.
    pub fn prev_correlation(&self) -> u64 {
        self.correlation.load(Ordering::Acquire)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
mod tests {
    use super::super::fixture::forward_to_envelope_sender;
    use super::super::identity::BindingIdentity;
    use super::*;
    use crate::actor::native::envelope::Envelope;
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// `prev_correlation` returns 0 before any send and tracks the
    /// monotonic counter as the eager send mints new ids.
    #[test]
    fn prev_correlation_tracks_eager_send_minting() {
        let (registry, mailer) = bare_substrate();
        let (tx, _rx) = mpsc::channel::<Envelope>();
        // Register a sink so push routes somewhere instead of
        // hitting the unknown-recipient warn.
        registry.register_inbox(&boot_authority(), "test.sink", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.sink").unwrap();

        let transport = NativeBinding::new_for_test(mailer, MailboxId(99));
        assert!(
            matches!(&transport.identity, BindingIdentity::Untyped { mailbox: MailboxId(99), carry: 99, .. }),
            "test bindings must have one untyped identity source",
        );
        assert!(transport.runtime_identity().is_none(), "test bindings must remain logically untyped");
        assert!(transport.spawner().is_none(), "untyped test bindings must not be able to spawn");
        assert_eq!(transport.carry(), 99, "untyped relative resolution keeps the depth-1 carry");
        assert_eq!(transport.parent_mailbox(), None, "legacy untyped bindings have no logical parent");

        assert_eq!(transport.prev_correlation(), 0);
        transport.push_envelope_returning_root_before_push(
            OutboundSend {
                recipient: recipient.0,
                kind: 1,
                bytes: &[],
                attachments: &[],
                count: 1,
                parent_mail: None,
                inherited_root: None,
            },
            |_| {},
        );
        assert_eq!(transport.prev_correlation(), 1);
        transport.push_envelope_returning_root_before_push(
            OutboundSend {
                recipient: recipient.0,
                kind: 1,
                bytes: &[],
                attachments: &[],
                count: 1,
                parent_mail: None,
                inherited_root: None,
            },
            |_| {},
        );
        assert_eq!(transport.prev_correlation(), 2);
    }

    #[test]
    fn eager_identity_hook_runs_before_inline_publication() {
        use crate::mail::registry::MailDispatch;

        let (registry, mailer) = bare_substrate();
        let published = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&published);
        let (tx, rx) = mpsc::channel();
        let recipient = registry
            .register_inline(
                &boot_authority(),
                "test.binding.before-push",
                Arc::new(move |_dispatch: MailDispatch<'_>| {
                    tx.send(*observed.lock().unwrap()).unwrap();
                }),
            )
            .id();
        let binding = NativeBinding::new_for_test(mailer, MailboxId(0xB4_221E));

        let mail_id = binding.push_envelope_returning_root_before_push(
            OutboundSend {
                recipient: recipient.0,
                kind: KindId(1).0,
                bytes: &[],
                attachments: &[],
                count: 1,
                parent_mail: None,
                inherited_root: None,
            },
            |mail_id| {
                published.lock().unwrap().replace(mail_id);
            },
        );

        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), Some(mail_id));
    }
}
