//! The eager send path — mint a correlation, stamp the lineage, push straight
//! through the mailer — and the correlation counter it advances.

use std::sync::atomic::Ordering;

use super::NativeBinding;
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
    /// Push a typed payload at `recipient`. Mints a fresh correlation
    /// id (atomic monotonic counter), wraps the bytes in a [`Mail`]
    /// with `SourceAddr::Component(self.self_mailbox)` so any reply
    /// routes back here, and pushes through the shared
    /// `Arc<Mailer>`. Returns `0` (channel-send failures collapse to
    /// the same scalar — there is no FFI surface here to differentiate).
    ///
    /// Stamps `MailId`/`root`/`parent_mail` as a chassis-root send
    /// (no inheritance). Per-handler ctxs that have an in-flight mail
    /// to inherit from go through [`Self::send_mail_with_lineage`]
    /// instead — the four-arg shape preserves wire stability for the
    /// FFI bridge and chassis-side log push paths that do not carry
    /// a per-handler context.
    pub fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
        self.send_mail_with_lineage(recipient, kind, bytes, count, None, None)
    }

    /// ADR-0080 §1 / §5: variant of [`Self::send_mail`] that accepts
    /// the in-flight handler's lineage so the outgoing [`Mail`] picks
    /// up the correct `parent_mail` and inherited `root`. The
    /// per-handler [`super::ctx::NativeCtx`](crate::actor::native::ctx::NativeCtx)'s
    /// [`aether_actor::model::ctx::MailSender`] impl reads from its
    /// `in_flight_mail_id()` / `in_flight_root()` accessors and threads
    /// them in.
    ///
    /// `parent_mail = None` and `inherited_root = None` mean
    /// chassis-root: the outgoing mail's `MailId` becomes its own
    /// `root`, marking the start of a new causal chain.
    pub fn send_mail_with_lineage(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
    ) -> u32 {
        let _ = self.push_envelope_returning_root(recipient, kind, bytes, count, parent_mail, inherited_root);
        0
    }

    /// Like [`Self::send_mail_with_lineage`] but returns the minted
    /// `MailId` (== the new root when `inherited_root.is_none()`) so the
    /// caller can subscribe to its settlement via the chassis
    /// [`crate::chassis::settlement::SettlementRegistry`].
    ///
    /// Same semantics as the `u32`-returning variant; the success-path
    /// `0` was vestigial at this layer (channel-send failures collapse to
    /// the same scalar).
    ///
    /// # Panics
    /// Panics if the `pending_recipients` mutex is poisoned — fail-fast
    /// per ADR-0063: a poisoned mutex means a prior holder panicked
    /// inside the guard, which is itself a substrate-level invariant
    /// violation.
    pub fn push_envelope_returning_root(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
    ) -> MailId {
        self.push_envelope_returning_root_before_push(
            recipient,
            kind,
            bytes,
            count,
            parent_mail,
            inherited_root,
            |_| {},
        )
    }

    /// Mint an eager envelope's identity, expose it to `before_push`, then
    /// publish the mail. The activation barrier uses this narrow hook to make
    /// its exact identity visible before another owner worker can consume it.
    #[allow(
        clippy::too_many_arguments,
        reason = "the hook preserves the established eager-envelope dimensions and adds one ordering callback"
    )]
    pub(in crate::actor::native) fn push_envelope_returning_root_before_push(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
        before_push: impl FnOnce(MailId),
    ) -> MailId {
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
        let mail = Mail::new(recipient_id, KindId(kind), bytes.to_vec(), count).with_reply_to(reply_to).with_lineage(
            mail_id,
            root,
            parent_mail,
        );
        self.mailer.push(mail);
        mail_id
    }

    /// Correlation id the substrate minted for this actor's most
    /// recent `send_mail` (ADR-0042). `0` before any send. Universal
    /// — every send mints a correlation; a handler stashes it and
    /// matches it against the inbound reply's correlation to pair a
    /// reply with the request it sent.
    pub fn prev_correlation(&self) -> u64 {
        self.correlation.load(Ordering::Acquire)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
#[allow(clippy::disallowed_methods)] // test scaffolding — the flat hash is the negative control a send must miss
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
    /// monotonic counter as `send_mail` mints new ids.
    #[test]
    fn prev_correlation_tracks_send_mail_minting() {
        let (registry, mailer) = bare_substrate();
        let (tx, _rx) = mpsc::channel::<Envelope>();
        // Register a sink so push routes somewhere instead of
        // hitting the unknown-recipient warn.
        registry.register_inbox(&boot_authority(), "test.sink", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.sink").unwrap();

        let transport = NativeBinding::new_for_test(mailer, MailboxId(99));
        assert!(
            matches!(&transport.identity, BindingIdentity::Untyped { mailbox: MailboxId(99), carry: 99 }),
            "test bindings must have one untyped identity source",
        );
        assert!(transport.runtime_identity().is_none(), "test bindings must remain logically untyped");
        assert!(transport.spawner().is_none(), "untyped test bindings must not be able to spawn");
        assert_eq!(transport.carry(), 99, "untyped relative resolution keeps the depth-1 carry");

        assert_eq!(transport.prev_correlation(), 0);
        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        assert_eq!(transport.prev_correlation(), 1);
        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        assert_eq!(transport.prev_correlation(), 2);
    }

    #[test]
    fn eager_identity_hook_runs_before_inline_publication() {
        use crate::mail::registry::MailDispatch;

        let (registry, mailer) = bare_substrate();
        let published = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&published);
        let (tx, rx) = mpsc::channel();
        let recipient = registry.register_inline(
            &boot_authority(),
            "test.binding.before-push",
            Arc::new(move |_dispatch: MailDispatch<'_>| {
                tx.send(*observed.lock().unwrap()).unwrap();
            }),
        );
        let binding = NativeBinding::new_for_test(mailer, MailboxId(0xB4_221E));

        let mail_id =
            binding.push_envelope_returning_root_before_push(recipient.0, KindId(1).0, &[], 1, None, None, |mail_id| {
                published.lock().unwrap().replace(mail_id);
            });

        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), Some(mail_id));
    }

    // ADR-0119: the per-impl `Singleton::resolve` override (the
    // "folded-child singleton") is retired — resolution is the chosen
    // resolver, not an overridable method. `ctx.actor` carry-folding is now
    // exercised through the `Embedded` resolver (aether-actor's resolve unit
    // tests + the send-path test below), so the dedicated own-child ctx.actor
    // test is dropped rather than retargeted.

    /// ADR-0099 §5 own-child path through the generic `MailSender::send`
    /// surface: `send::<R>` resolves the receiver through
    /// `R::resolve(caller_carry)` — the same lineage-aware path
    /// `ctx.actor::<R>()` walks — so a parent at carry `C` sending by
    /// bare type lands on `fold(C, ActorId::singleton(NAMESPACE))`, not
    /// the flat `hash(NAMESPACE)`. The send-path analogue of
    /// `ctx_actor_folds_own_child_singleton_onto_caller_carry`, closing
    /// the divergence between the two send forms (#1550).
    #[test]
    fn mailsender_send_routes_through_resolve_not_flat_hash() {
        use crate::actor::native::ctx::NativeCtx;
        use aether_actor::model::HandlesKind;
        use aether_actor::{Addressable, Embedded, MailSender};
        use aether_data::mailbox_id_from_name;
        use aether_kinds::Tick;

        struct Child;
        impl Addressable for Child {
            const NAMESPACE: &'static str = "test.send_fold.child";
            // ADR-0119: `Embedded` folds the caller carry under the embed
            // scope, so a different caller carry resolves to a different
            // mailbox — a carry-dependent target distinct from the flat hash.
            // A send landing here proves the carry was threaded through
            // `resolve` rather than dropped for `mailbox_id_from_name`.
            type Resolver = Embedded;
        }
        impl HandlesKind<Tick> for Child {}

        let (registry, mailer) = bare_substrate();
        let parent_carry = 0x0BAD_F00D_u64;

        let resolved = <Child as Addressable>::resolve(parent_carry, ());
        let flat = mailbox_id_from_name("test.send_fold.child");
        assert_ne!(resolved, flat, "fixture: the carry-derived id must differ from the flat hash");

        // Capture at both candidate recipients: the carry-derived id the
        // send must target, and the flat depth-1 hash the pre-fix path used.
        let (resolved_tx, resolved_rx) = mpsc::channel::<Envelope>();
        let (flat_tx, flat_rx) = mpsc::channel::<Envelope>();
        registry
            .try_register_inbox_with_id(
                &boot_authority(),
                resolved,
                "test.send_fold.resolved",
                forward_to_envelope_sender(resolved_tx),
            )
            .expect("register resolved sink");
        registry
            .try_register_inbox_with_id(
                &boot_authority(),
                flat,
                "test.send_fold.flat",
                forward_to_envelope_sender(flat_tx),
            )
            .expect("register flat sink");

        let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(parent_carry)));
        {
            let mut ctx = NativeCtx::new(&transport, Source::NONE, MailId::NONE, MailId::NONE);
            <NativeCtx as MailSender>::send::<Child, Tick>(&mut ctx, &Tick);
            // ctx drops here → `flush_outbound` routes the buffered send.
        }

        assert!(resolved_rx.try_recv().is_ok(), "send must route to Child::resolve(carry), the carry-derived id");
        assert!(flat_rx.try_recv().is_err(), "send must NOT route to the flat hash(NAMESPACE)");
    }
}
