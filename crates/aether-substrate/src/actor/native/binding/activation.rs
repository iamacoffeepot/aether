//! The staged-activation hold (ADR-0165): buffered lifecycle effects stay
//! local until the registry owner promotes the route to `Live`, then publish
//! together — or are rejected together when the activation never lands.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::NativeBinding;
use super::pending::{ComponentOrigin, PendingMail, PendingPayload};
use crate::mail::{Mail, MailRef, MailboxId};

impl NativeBinding {
    /// Offer one fully-stamped wasm component mail to the staged activation
    /// hold. The activation flag and append are observed under the same lock:
    /// a concurrent release either drains this mail or makes the caller route
    /// it eagerly against the already-`Live` route.
    ///
    /// Settlement is bumped only when the hold accepts the mail. A rejected
    /// mail remains wholly owned by the caller, which performs the ordinary
    /// eager `record_sent` + dispatch path. `None` means accepted; `Some(mail)`
    /// returns a rejected offer unchanged.
    #[cfg(feature = "wasm")]
    pub(crate) fn try_hold_component_mail(&self, mail: Mail, sender: MailboxId) -> Option<Mail> {
        if !self.activation_held.load(Ordering::Acquire) {
            return Some(mail);
        }
        let mut buffer = self.outbound.lock().expect("outbound buffer poisoned; fail-fast per ADR-0063");
        if !buffer.activation_held {
            return Some(mail);
        }

        self.mailer.record_sent_inflight(mail.root);
        if buffer.construct_start.is_none() {
            buffer.construct_start = Some(self.mailer.now_nanos());
        }
        let Mail { recipient, kind, payload, count, reply_to, mail_id, root, parent_mail } = mail;
        buffer.component_origins.push(ComponentOrigin { mail_id, sender });
        buffer.mails.push(PendingMail {
            recipient: recipient.0,
            kind: kind.0,
            payload: PendingPayload::Prebuilt(payload),
            count,
            reply_to,
            mail_id,
            root,
            parent_mail,
        });
        None
    }

    /// Quarantine lifecycle-authored buffered work while a prepared actor is
    /// wired but not yet authoritatively `Live`.
    pub(in crate::actor::native) fn hold_outbound_for_activation(&self) {
        let mut buffer = self.outbound.lock().expect("outbound buffer poisoned; fail-fast per ADR-0063");
        assert!(!buffer.activation_held, "one staged activation owns the outbound hold");
        assert!(
            !buffer.blob_open
                && buffer.mails.is_empty()
                && buffer.component_origins.is_empty()
                && buffer.births.is_empty()
                && buffer.owner_batches.is_empty(),
            "a prepared actor enters wire with an empty outbound window"
        );
        buffer.activation_held = true;
        self.activation_held.store(true, Ordering::Release);
    }

    /// Publish the quarantined lifecycle suffix after the registry owner has
    /// installed the `Live` route. The actor remains unwakeable while this
    /// runs, preserving one logical producer for the ring.
    pub(in crate::actor::native) fn release_outbound_after_activation(&self) {
        let mut buffer = self.outbound.lock().expect("outbound buffer poisoned; fail-fast per ADR-0063");
        assert!(buffer.activation_held, "only a staged activation can release the outbound hold");
        buffer.activation_held = false;
        self.activation_held.store(false, Ordering::Release);
        drop(buffer);
        self.flush_outbound_inner();
    }

    /// Reject every buffered effect accumulated by `wire` and `unwire` when a
    /// staged activation never reaches `Live`. Mail settlement bumps are
    /// balanced locally, prepared births reject at their execution homes, and
    /// deferred owner completions abandon their held actor work.
    pub(in crate::actor::native) fn discard_outbound_after_activation(&self) {
        let (ring, mails, births, owner_batches) = {
            let mut buffer = self.outbound.lock().expect("outbound buffer poisoned; fail-fast per ADR-0063");
            assert!(buffer.activation_held, "only a staged activation can discard the outbound hold");
            buffer.activation_held = false;
            self.activation_held.store(false, Ordering::Release);
            if buffer.blob_open {
                if let Some(ring) = buffer.ring.as_ref() {
                    ring.seal();
                }
                buffer.blob_open = false;
            }
            buffer.construct_start = None;
            buffer.component_origins.clear();
            (
                buffer.ring.as_ref().map(Arc::clone),
                buffer.mails.drain(..).collect::<Vec<_>>(),
                buffer.births.drain(..).collect::<Vec<_>>(),
                buffer.owner_batches.drain(..).collect::<Vec<_>>(),
            )
        };

        for pending in mails {
            let PendingMail { payload, mail_id, root, .. } = pending;
            if let PendingPayload::InRing(location) = payload {
                drop(MailRef::in_ring(
                    Arc::clone(ring.as_ref().expect("ring exists once an InRing mail was minted")),
                    location,
                ));
            }
            self.mailer.record_finished(mail_id, root);
        }
        drop(births);
        drop(owner_batches);
    }
}

#[cfg(all(test, feature = "wasm"))]
mod tests {
    use super::super::fixture::{component_ctx_with_binding, forward_to_envelope_sender};
    use super::*;
    use crate::actor::native::envelope::Envelope;
    use crate::mail::{KindId, MailId, SourceAddr};
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    /// ADR-0165 interleaving: guest `wire` mail stays behind the activation
    /// hold, then release publishes it with the exact component dispatch
    /// metadata and settlement lineage it would have carried while Live.
    #[cfg(feature = "wasm")]
    #[test]
    fn activation_hold_releases_component_mail_with_original_dispatch_shape() {
        let (registry, mailer) = bare_substrate();
        let sender = MailboxId(0x0041_4501);
        let (sender_tx, _sender_rx) = mpsc::channel::<Envelope>();
        registry
            .try_register_inbox_with_id(
                &boot_authority(),
                sender,
                "test.activation.sender",
                forward_to_envelope_sender(sender_tx),
            )
            .expect("register component sender");
        let (recipient_tx, recipient_rx) = mpsc::channel::<Envelope>();
        let recipient = registry.register_inbox(
            &boot_authority(),
            "test.activation.recipient",
            forward_to_envelope_sender(recipient_tx),
        );
        let (ctx, binding) = component_ctx_with_binding(Arc::clone(&registry), Arc::clone(&mailer), sender);
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let parent = MailId::new(MailboxId(0x0041_4502), 7);
        let root = MailId::new(MailboxId(0x0041_4503), 9);
        let kind = KindId(0x0041_4504);

        binding.hold_outbound_for_activation();
        ctx.set_in_flight(parent, root);
        ctx.send(recipient, kind, vec![1, 2, 3], 2, sender);

        assert!(recipient_rx.try_recv().is_err(), "wire mail must remain quarantined before activation");
        assert_eq!(counter.live_roots(), 1, "accepted wire mail holds its inherited root open");

        binding.release_outbound_after_activation();
        let envelope = recipient_rx.try_recv().expect("release routes the retained wire mail exactly once");
        assert_eq!(envelope.kind, kind);
        assert_eq!(envelope.payload.bytes(), &[1, 2, 3]);
        assert_eq!(envelope.count, 2);
        assert_eq!(envelope.sender.addr, SourceAddr::Component(sender));
        assert_eq!(envelope.sender.correlation_id, 1);
        assert_eq!(envelope.mail_id, MailId::new(sender, 1));
        assert_eq!(envelope.root, root);
        assert_eq!(envelope.parent_mail, Some(parent));
        assert_eq!(envelope.recipient, recipient);
        assert_eq!(envelope.origin.as_deref(), Some("test.activation.sender"));
        assert!(recipient_rx.try_recv().is_err(), "release must not duplicate retained mail");

        mailer.record_finished(envelope.mail_id, envelope.root);
        assert_eq!(counter.live_roots(), 0, "recipient completion balances the retained send");
    }

    /// ADR-0165 at the release boundary: one activation window that buffered
    /// both a native lifecycle send and a guest-authored component send
    /// publishes them in send order, each under its own origin — the native
    /// mail through the mailer (ADR-0011 `origin: None`), the component mail
    /// through the direct component dispatch that supplies the guest's
    /// canonical name.
    ///
    /// The bug this catches is the flush collapsing the two into one route to
    /// shed the per-mail route tag (iamacoffeepot/aether#4178): folding the
    /// window into the blob/mailer path alone still delivers every mail, so
    /// only the origin distinguishes a correct split from a lossy one. It
    /// equally catches the origin record being keyed to the wrong mail, which
    /// would attribute the component name to the native send.
    #[cfg(feature = "wasm")]
    #[test]
    fn activation_release_gives_native_and_component_mail_their_own_origins_in_send_order() {
        let (registry, mailer) = bare_substrate();
        let component = MailboxId(0x0041_4531);
        let (component_tx, _component_rx) = mpsc::channel::<Envelope>();
        registry
            .try_register_inbox_with_id(
                &boot_authority(),
                component,
                "test.mixed.component",
                forward_to_envelope_sender(component_tx),
            )
            .expect("register component trampoline");
        // Issue 1987: an inline child's mail is attributed to the child's own
        // address, so the guest-carried identity is deliberately not the
        // binding's mailbox — that is what makes the origin discriminating.
        let (child_tx, _child_rx) = mpsc::channel::<Envelope>();
        let child =
            registry.register_inbox(&boot_authority(), "test.mixed.child", forward_to_envelope_sender(child_tx));
        let (recipient_tx, recipient_rx) = mpsc::channel::<Envelope>();
        let recipient = registry.register_inbox(
            &boot_authority(),
            "test.mixed.recipient",
            forward_to_envelope_sender(recipient_tx),
        );
        let (ctx, binding) = component_ctx_with_binding(Arc::clone(&registry), Arc::clone(&mailer), component);
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        binding.hold_outbound_for_activation();
        binding.push_envelope_buffered(recipient.0, 0x0041_4532, &[7, 7], 1, None, None);
        ctx.send(recipient, KindId(0x0041_4533), vec![9], 1, child);
        assert!(recipient_rx.try_recv().is_err(), "a held window publishes nothing before activation");
        assert_eq!(counter.live_roots(), 2, "both held sends record one in-flight root each");

        binding.release_outbound_after_activation();

        let native = recipient_rx.try_recv().expect("release publishes the buffered native send");
        assert_eq!(native.payload.bytes(), &[7, 7], "the native send releases first, in buffer order");
        assert_eq!(native.origin, None, "native mail routes through the mailer, which stamps no component origin");
        let guest = recipient_rx.try_recv().expect("release publishes the held component send");
        assert_eq!(guest.payload.bytes(), &[9], "the component send releases after the native one it followed");
        assert_eq!(
            guest.origin.as_deref(),
            Some("test.mixed.child"),
            "component mail keeps the direct dispatch that names its guest-carried origin"
        );
        assert!(recipient_rx.try_recv().is_err(), "release publishes each held mail exactly once");

        mailer.record_finished(native.mail_id, native.root);
        mailer.record_finished(guest.mail_id, guest.root);
        assert_eq!(counter.live_roots(), 0, "recipient completion balances both held sends");
    }

    /// A failed staged activation rejects guest `wire` mail locally: no route
    /// becomes observable, and the eager settlement bump is balanced once.
    #[cfg(feature = "wasm")]
    #[test]
    fn activation_discard_rejects_component_mail_and_balances_settlement() {
        let (registry, mailer) = bare_substrate();
        let sender = MailboxId(0x0041_4511);
        let (recipient_tx, recipient_rx) = mpsc::channel::<Envelope>();
        let recipient = registry.register_inbox(
            &boot_authority(),
            "test.activation.reject",
            forward_to_envelope_sender(recipient_tx),
        );
        let (ctx, binding) = component_ctx_with_binding(registry, Arc::clone(&mailer), sender);
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        binding.hold_outbound_for_activation();
        ctx.send(recipient, KindId(0x0041_4512), vec![4, 5], 1, MailboxId::NONE);
        assert!(recipient_rx.try_recv().is_err(), "rejected wire mail never escapes before discard");
        assert_eq!(counter.live_roots(), 1, "accepted wire mail records one in-flight send");

        binding.discard_outbound_after_activation();
        assert!(recipient_rx.try_recv().is_err(), "discard must not route retained component mail");
        assert_eq!(counter.live_roots(), 0, "discard balances the retained send exactly once");
    }

    /// Outside staged activation the same binding rejects the hold offer and
    /// preserves the ordinary synchronous component route.
    #[cfg(feature = "wasm")]
    #[test]
    fn live_component_mail_remains_eager_with_binding_installed() {
        let (registry, mailer) = bare_substrate();
        let sender = MailboxId(0x0041_4521);
        let (recipient_tx, recipient_rx) = mpsc::channel::<Envelope>();
        let recipient = registry.register_inbox(
            &boot_authority(),
            "test.activation.live",
            forward_to_envelope_sender(recipient_tx),
        );
        let (ctx, _binding) = component_ctx_with_binding(registry, Arc::clone(&mailer), sender);
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        ctx.send(recipient, KindId(0x0041_4522), vec![6], 1, MailboxId::NONE);
        let envelope = recipient_rx.try_recv().expect("Live component send remains eager");
        assert_eq!(envelope.payload.bytes(), &[6]);
        assert_eq!(counter.live_roots(), 1);

        mailer.record_finished(envelope.mail_id, envelope.root);
        assert_eq!(counter.live_roots(), 0);
    }
}
