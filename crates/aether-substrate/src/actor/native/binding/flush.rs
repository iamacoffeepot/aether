//! Sealing the outbound window and routing it (ADR-0087): one ring blob, one
//! mail reference per buffered mail, then the cursor-shared blob producer or
//! the per-mail fallback.

use std::sync::Arc;

use super::NativeBinding;
use super::outbound::OutboundBuffer;
use super::pending::{ComponentOrigin, PendingBirthWork, PendingOwnerBatchWork, PendingPayload, component_origin};
use crate::mail::registry::effect::{EffectBatch, RegistryEffect};
use crate::mail::{KindId, Mail, MailRef, MailboxId};

#[cfg(feature = "wasm")]
use crate::actor::wasm::component::ComponentCtx;

impl NativeBinding {
    /// ADR-0087 / 2c: seal the open ring blob and route the buffered
    /// mail. Called at handler end (via [`super::ctx::NativeCtx`](crate::actor::native::ctx::NativeCtx)'s
    /// `Drop`). A no-op when nothing is buffered.
    ///
    /// The payloads are already in the ring (written by
    /// `push_envelope_buffered` as each send happened) or copied out to
    /// `Owned`; this just [`seal`](crate::mail::ring::MailRing::seal)s the blob — publishing
    /// each in-ring mail's lock — and mints one [`MailRef`] per pending
    /// entry: [`MailRef::InRing`] for ring-resident payloads (the
    /// recipient reads them in place), [`MailRef::Owned`] for the
    /// copy-out fallback. The route metadata is identical for both, so
    /// the dispatch read path is unchanged.
    ///
    /// The buffer lock is released **before** routing: `Mailer::push` can
    /// run an inline handler synchronously, and holding the lock across
    /// arbitrary handler code would be a needless contention/re-entrancy
    /// hazard. A single drain suffices — the buffer is written only by
    /// this actor's per-handler send path, never re-entrantly during
    /// routing (inline handlers receive a `MailDispatch`, not a
    /// buffering `NativeCtx`).
    ///
    /// ADR-0087 Phase 3b: when a pool [`WakeSink`](crate::scheduler::WakeSink)
    /// is wired (every production binding — derived from the chassis
    /// `Spawner`), the whole blob is pushed as **one** `BlobWork` work
    /// item rather than routed per mail, so a fan-out of N costs one
    /// deque push + an inline demux instead of N pushes + up to N
    /// parked-worker wakeups.
    /// A binding with no `Spawner` (test transports built via
    /// [`Self::new_for_test`]) keeps the eager per-mail route.
    ///
    /// # Panics
    /// Panics if the outbound-buffer mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub fn flush_outbound(&self) {
        self.flush_outbound_inner();
    }

    /// Seal the open blob, mint a [`MailRef`] per buffered mail, and
    /// route. Folds the blob into this actor's cursor-shared
    /// [`BlobWork`](crate::actor::native::blob::work::BlobWork) when a pool [`Spawner`](crate::Spawner) is wired,
    /// else routes per mail (test bindings without a spawner); a no-op
    /// when nothing is buffered.
    ///
    /// iamacoffeepot/aether#1150: this is the frame's flush-begin
    /// instant. Once the buffer is known non-empty, one `now_nanos` read
    /// stamps `flush_begin`, and every mail in the frame emits its
    /// deferred `Sent` trace event against that shared anchor (the
    /// per-send call site only bumped `in_flight`). Anchoring `Sent`
    /// here, not at the send call, drops the smear of "the rest of the
    /// handler that ran after the send" from the producer-side span. The
    /// clock read sits behind the emptiness check so a no-send handler
    /// return stays free.
    pub(super) fn flush_outbound_inner(&self) {
        let flush_begin;
        // iamacoffeepot/aether#1158: the construct-start anchor stamped
        // when this outbound window opened (a native blob send or a retained
        // component send). Read it here and reset for the next window; fall
        // back to `flush_begin` (construct ≈ 0) on the impossible `None` so
        // the field is never a wire hole.
        let construct_start;
        let (routed, component_origins, births, owner_batches): (
            Vec<Mail>,
            Vec<ComponentOrigin>,
            Vec<PendingBirthWork>,
            Vec<PendingOwnerBatchWork>,
        ) = {
            let mut buf = self.outbound.lock().expect("outbound buffer poisoned; fail-fast per ADR-0063");
            if buf.activation_held {
                return;
            }
            // Seal the open blob first (publishes the in-ring locks), so a
            // `MailRef::InRing` minted below reads a finalized header.
            if buf.blob_open {
                if let Some(ring) = buf.ring.as_ref() {
                    ring.seal();
                }
                buf.blob_open = false;
            }
            if buf.mails.is_empty() && buf.births.is_empty() && buf.owner_batches.is_empty() {
                // Reset the stale anchor so the next window re-stamps.
                buf.construct_start = None;
                return;
            }
            flush_begin = self.mailer.now_nanos();
            // Take the anchor and reset so the next blob re-stamps.
            construct_start = buf.construct_start.take().unwrap_or(flush_begin);
            let OutboundBuffer { ring, mails, .. } = &mut *buf;
            let ring = ring.as_ref();
            let routed = mails
                .drain(..)
                .map(|p| {
                    let payload = match p.payload {
                        PendingPayload::InRing(loc) => {
                            MailRef::in_ring(Arc::clone(ring.expect("ring exists once an InRing mail was minted")), loc)
                        }
                        PendingPayload::Owned(bytes) => MailRef::from(bytes),
                        #[cfg(feature = "wasm")]
                        PendingPayload::Prebuilt(payload) => payload,
                    };
                    Mail::new(MailboxId(p.recipient), KindId(p.kind), payload, p.count)
                        .with_reply_to(p.reply_to)
                        .with_lineage(p.mail_id, p.root, p.parent_mail)
                })
                .collect();
            (
                routed,
                buf.component_origins.drain(..).collect(),
                buf.births.drain(..).collect(),
                buf.owner_batches.drain(..).collect(),
            )
        };

        // iamacoffeepot/aether#1150: emit each buffered mail's deferred
        // `Sent` trace event against the shared flush-begin anchor before
        // routing (the lock is already released — `push_trace_ring` runs
        // off the actor's own ring). `in_flight` was bumped eagerly at
        // the send call, so this is purely the trace-event half.
        let self_mailbox = self.self_mailbox();
        for mail in &routed {
            self.mailer.record_sent_event_at(
                mail.mail_id,
                mail.root,
                mail.parent_mail,
                component_origin(&component_origins, mail.mail_id).unwrap_or(self_mailbox),
                mail.recipient,
                mail.kind,
                construct_start,
                flush_begin,
            );
        }

        if !owner_batches.is_empty() {
            let registry = self.spawner.as_ref().expect("staged owner batches require a spawner").registry();
            for work in owner_batches {
                let _ = registry.submit_deferred(work.batch.into_effects(), work.completion);
            }
        }

        let routed = if births.is_empty() {
            routed
        } else {
            let mut routed = routed.into_iter().map(Some).collect::<Vec<_>>();
            let mut effects = Vec::with_capacity(births.len());
            for mut birth in births {
                for mail in routed.iter_mut().skip(birth.after_mail) {
                    if mail.as_ref().is_some_and(|mail| mail.recipient == birth.recipient) {
                        let mail = mail.take().expect("matched same-flush child mail remains present");
                        birth.commit.retain_after_init(mail);
                    }
                }
                effects.push(RegistryEffect::PreparedSpawn(birth.commit));
            }
            let registry = self.spawner.as_ref().expect("staged births require a spawner").registry();
            drop(registry.submit(EffectBatch::new(effects)));
            routed.into_iter().flatten().collect()
        };

        self.route_pending_mails(routed, &component_origins);
    }

    /// ADR-0087 / iamacoffeepot/aether#1137: fold native mail into this
    /// actor's single active cursor-shared blob. A staged component mail keeps
    /// the direct component route that supplies its canonical origin; the
    /// activation-only mixed window routes sequentially so send order remains
    /// explicit.
    fn route_pending_mails(&self, routed: Vec<Mail>, component_origins: &[ComponentOrigin]) {
        // Fold the blob into this
        // actor's single active cursor-shared blob (recipient-grouped,
        // cooperatively drained, broadcast-recruited for wide fan-outs)
        // when a pool sink is wired. Otherwise route per mail (a test
        // binding with no `Spawner`, or the activation window that admitted
        // guest-authored mail). The window arrives as the `Vec<Mail>` the
        // producer consumes, so the steady-state path hands it straight on.
        if self.spawner.is_some() && component_origins.is_empty() {
            let mut guard = self.blob_producer.lock().expect("blob_producer poisoned; fail-fast per ADR-0063");
            let producer = guard.get_or_insert_with(|| {
                let sink = self.spawner.as_ref().expect("spawner present in this branch").wake_sink().clone();
                super::blob::work::BlobProducer::new(Arc::clone(&self.mailer), sink)
            });
            producer.flush(routed);
        } else {
            for mail in routed {
                match component_origin(component_origins, mail.mail_id) {
                    Some(sender) => self.dispatch_component_mail(mail, sender),
                    None => self.mailer.push(mail),
                }
            }
        }
    }

    /// Publish one released component mail under the canonical origin the
    /// guest sent it with, preserving the direct dispatch an ordinary Live
    /// component send takes.
    #[cfg(feature = "wasm")]
    fn dispatch_component_mail(&self, mail: Mail, sender: MailboxId) {
        ComponentCtx::dispatch_routed_mail(self.mailer.registry(), &self.mailer, mail, sender);
    }

    /// Without the wasm trampoline nothing records a component origin, so no
    /// mail reaches here; the mailer push keeps the arm honest either way.
    #[cfg(not(feature = "wasm"))]
    fn dispatch_component_mail(&self, mail: Mail, _sender: MailboxId) {
        self.mailer.push(mail);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
mod tests {
    use super::super::fixture::forward_to_envelope_sender;
    use super::super::outbound::ACTOR_RING_BYTES;
    use super::*;
    use crate::actor::native::envelope::Envelope;
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// 2b: the buffered send path holds mail until flush, then forms one
    /// blob and routes each mail to its recipient with bytes + kind
    /// intact. Nothing reaches the sink before `flush_outbound`.
    #[test]
    fn buffered_sends_route_only_after_flush() {
        let (registry, mailer) = bare_substrate();
        let (tx, rx) = mpsc::channel::<Envelope>();
        registry.register_inbox(&boot_authority(), "test.sink", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.sink").unwrap();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(0x5151));

        transport.push_envelope_buffered(recipient.0, 7, &[1, 2, 3], 1, None, None);
        transport.push_envelope_buffered(recipient.0, 9, &[4, 5], 1, None, None);
        assert!(rx.try_recv().is_err(), "buffered sends must not route before flush");

        transport.flush_outbound();
        let a = rx.try_recv().expect("first mail delivered after flush");
        let b = rx.try_recv().expect("second mail delivered after flush");
        assert_eq!(a.payload.bytes(), &[1, 2, 3]);
        assert_eq!(a.kind, KindId(7));
        assert_eq!(b.payload.bytes(), &[4, 5]);
        assert_eq!(b.kind, KindId(9));
        // Buffer drained — a second flush is a no-op.
        transport.flush_outbound();
        assert!(rx.try_recv().is_err());
    }

    /// 2b: a payload larger than the per-actor ring degrades to the
    /// `Owned` copy-out valve rather than panicking, still delivering the
    /// bytes intact (the large-payload zero-copy path is deferred).
    #[test]
    fn buffered_oversized_payload_flushes_via_copy_out() {
        let (registry, mailer) = bare_substrate();
        let (tx, rx) = mpsc::channel::<Envelope>();
        registry.register_inbox(&boot_authority(), "test.sink", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.sink").unwrap();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(0x6262));

        // Larger than the whole ring — never fits, so the valve copies out.
        let big = vec![0xABu8; ACTOR_RING_BYTES + 4096];
        transport.push_envelope_buffered(recipient.0, 3, &big, 1, None, None);
        transport.flush_outbound();

        let env = rx.try_recv().expect("oversized mail still delivered via copy-out");
        assert_eq!(env.payload.len(), big.len());
        assert_eq!(env.payload.bytes(), &big[..]);
    }

    /// 2b: flushing an empty buffer is a no-op — the common idempotent
    /// case, since `NativeCtx::Drop` flushes every handler and most send
    /// nothing. Must not panic or allocate a ring.
    #[test]
    fn buffered_flush_empty_is_noop() {
        let (_registry, mailer) = bare_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(0x7373));
        transport.flush_outbound();
        transport.flush_outbound();
    }

    /// 2b load-bearing race: the producer flushes tagged blobs into its
    /// ring while consumer threads read each `InRing` payload in place
    /// and drop the envelope (RAII-releasing the blob lock). A reused
    /// region — the producer overwriting bytes a consumer is mid-read on
    /// — would surface as a tag mismatch. This lifts the 2a ring stress
    /// test onto the full 2b path: buffer → flush → route → mpsc →
    /// consumer drop.
    #[test]
    fn buffered_concurrent_flush_and_consumer_release() {
        use std::thread;

        let (registry, mailer) = bare_substrate();
        let (tx, rx) = mpsc::channel::<Envelope>();
        registry.register_inbox(&boot_authority(), "test.sink", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.sink").unwrap();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(0x9191));

        let rx = Arc::new(Mutex::new(rx));
        let done = Arc::new(AtomicBool::new(false));
        let consumed = Arc::new(AtomicU64::new(0));
        let n_consumers = 4;

        let consumers: Vec<_> = (0..n_consumers)
            .map(|_| {
                let rx = Arc::clone(&rx);
                let done = Arc::clone(&done);
                let consumed = Arc::clone(&consumed);
                thread::spawn(move || {
                    loop {
                        let got = {
                            let guard = rx.lock().expect("rx mutex poisoned");
                            guard.recv_timeout(Duration::from_millis(20))
                        };
                        match got {
                            Ok(env) => {
                                let bytes = env.payload.bytes();
                                let tag = bytes[0];
                                assert!(
                                    bytes.iter().all(|&b| b == tag),
                                    "decode-in-place saw a reused region: expected tag {tag}"
                                );
                                drop(env); // RAII release of the blob lock
                                consumed.fetch_add(1, Ordering::AcqRel);
                            }
                            // Empty for the timeout: exit only once the
                            // producer is done (channel fully drained).
                            Err(_) if done.load(Ordering::Acquire) => break,
                            Err(_) => {}
                        }
                    }
                })
            })
            .collect();

        let mut sent = 0u64;
        for i in 0..4_000u32 {
            let tag = (i & 0xff) as u8;
            let n = (i % 4 + 1) as usize;
            let payload = vec![tag; 8 + (i as usize % 24)];
            for _ in 0..n {
                transport.push_envelope_buffered(recipient.0, 7, &payload, 1, None, None);
                sent += 1;
            }
            transport.flush_outbound();
        }
        // All flushes returned synchronously, so every envelope is in the
        // channel before we signal done.
        done.store(true, Ordering::Release);
        for h in consumers {
            h.join().expect("consumer thread joins");
        }
        assert_eq!(consumed.load(Ordering::Acquire), sent, "every flushed mail must be consumed");
    }
}
