//! [`BlobWork`] — a handler's buffered fan-out as a single
//! work-stealing unit (ADR-0087 Phase 3b, iamacoffeepot/aether#1113;
//! claim-and-dispatch-direct demux, iamacoffeepot/aether#1135).
//!
//! Phase 2b/2c buffer a native handler's outbound mail into one sealed
//! ring blob; Phase 3a made per-worker deques the scheduler's queue.
//! 3b ties them together: instead of routing each buffered mail
//! eagerly (N pushes + up to N parked-worker wakeups for a fan-out of
//! N), [`crate::actor::native::NativeBinding::flush_outbound`] pushes
//! the whole blob as **one** `Drainable` onto the producing worker's
//! deque. A worker pops it (or an idle sibling steals it) and demuxes
//! its mail in **send order** (ADR-0087 §4, the order-safe half of the
//! iamacoffeepot/aether#1059 win):
//!
//! - **free** recipient (won the `Idle → Running` *seize* CAS, ref-free
//!   kind) → build the envelope and dispatch it **in place** on this
//!   worker via [`crate::scheduler::Drainable::seize_and_run`] — no inbox
//!   deposit, no `try_recv` repop, residence ≈ 0
//!   (iamacoffeepot/aether#1135 removed the 3b deposit+collect+repop
//!   round-trip).
//! - **busy** recipient (lost the seize), **non-`Pooled`** recipient (no
//!   slot to seize), or an **ADR-0045 ref kind** → deposit through
//!   today's [`Mailer::push`] → `route_mail`; the holder / woken cycle
//!   drains it (per-recipient FIFO preserved by the inbox's own order),
//!   and `route_mail` owns the ref handle walk / park.
//!
//! ## Chunked spill (Phase 3c, iamacoffeepot/aether#1116)
//!
//! 3b runs a whole fan-out blob inline on one worker — optimal latency,
//! but a heavy / wide fan-out serialises (the pre-blob path scattered the
//! leaves across workers). 3c caps the inline demux at `K` mails
//! ([`demux_chunk`], `AETHER_BLOB_DEMUX_CHUNK`): a blob wider than `K`
//! `split_off`s the remainder and pushes it as a stealable sub-blob
//! *before* demuxing its own `K` chunk, so an idle sibling can steal the
//! remainder while this worker runs its chunk (the throughput path); with
//! no idle worker the producer pops the remainder itself next loop and
//! continues in `K`-chunks (the latency path, LIFO-warm). The split is a
//! [`Vec::split_off`] of `Mail` handles — `MailRef`s (ring offsets / owned
//! boxes) are **moved, never copied**, so the sub-blob references the same
//! ring regions and the reclaim-lock counts travel with the moved refs.
//! (Consequence: a spilled sub-blob pins its producer ring region until
//! its chunk drains; under sustained heavy fan-out the 2a ring-full →
//! `Owned` valve absorbs the back-pressure — no producer block.)
//!
//! ## How the demux reuses the one router
//!
//! Both arms keep all routing concerns in one place. The **deposit** arm
//! is today's [`Mailer::push`] → `route_mail`, so the ADR-0045 ref-walk,
//! the `Inline` / `Dropped` / unknown-bubble-up arms, and that path's
//! settlement/trace brackets are inherited unchanged. The **direct**
//! arm runs the recipient's [`crate::scheduler::Drainable::seize_and_run`]
//! → `DispatcherSlot::dispatch_one`, the *same* per-envelope wrapper a
//! pooled `run_cycle` runs — `local::with_stamped`, `Received` /
//! `Finished` (incl. the iamacoffeepot/aether#1134 `t_enqueue` /
//! `enqueue_depth`), the `record_finished` settlement bracket, and the
//! `log.tail` / `trace.tail` framework arms — so the only thing it skips
//! is the mpsc deposit + `try_recv` repop. The demux resolves the
//! recipient's seize handle (and ref-schema) up front via
//! `Registry::route_lookup`, the same combined read `route_mail` uses,
//! and falls back to deposit whenever direct dispatch doesn't apply.
//!
//! ## Single-runner, order-safe scope (iamacoffeepot/aether#1135)
//!
//! This blob stays **one-shot, single-runner**: `run_cycle` takes the
//! mail out once (`mails.lock().take()`) and demuxes it on one worker in
//! send order. The parallel multi-worker demux (cursor-shared groups +
//! recruitment, which reorders across recipients — sound under the
//! ordering spine but real concurrency machinery) is deferred to
//! iamacoffeepot/aether#1137, gated on whether direct dispatch alone
//! closes the residence gap. The 3c [`Vec::split_off`] spill below is the
//! one stealable hand-off and is unchanged by #1135.
//!
//! ## Alternative not taken — explicit slot-demux registry (revisit?)
//!
//! The other shape (iamacoffeepot/aether#1113 design fork) was a
//! first-class `MailboxId → (SlotState, Weak<slot>)` table the blob
//! worker resolves directly, depositing via `ActorRegistry::live_sender`
//! and routing only non-`Inbox` recipients through `route_mail`. The
//! seize handle surfaced on the registry `Inbox` entry
//! (iamacoffeepot/aether#1135) is the lighter form of that idea — it adds
//! no second shared map (it rides the entry `route_lookup` already
//! resolves) and keeps `route_mail` as the single fallback router, so the
//! fast path and the deposit path don't drift. **Reconsider a dedicated
//! table only if the scheduler later needs first-class slot addressing
//! for other consumers** (affinity-as-data, slot introspection, NUMA
//! placement) — at which point its real consumers exist and it earns its
//! keep.

use std::any::Any;
use std::env;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::actor::native::Envelope;
use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SeizeHandle, WakeSink};

/// Inline-demux chunk cap K (ADR-0087 Phase 3c, iamacoffeepot/aether#1116).
/// A `BlobWork` of more than `K` mails demuxes the first `K` inline and
/// spills the remainder as a stealable sub-blob, so a wide fan-out
/// parallelises across idle workers (each chunk is at most `K` mails of
/// work before a steal point) instead of serialising on the one demuxing
/// worker. Read once from `AETHER_BLOB_DEMUX_CHUNK`; values `< 1` and
/// unparseable input fall back to the default.
///
/// **Default `8`**, set from the iamacoffeepot/aether#1116 K-sweep
/// (3-sample medians, 11 workers): fan-outs wider than 8 win — fanout-16
/// ~0.92× / fanout-32 ~0.84× hop-p50 vs the chunking-off 3b baseline, p99
/// flat-to-better — while fan-outs of `≤ 8` (the common case, and the
/// narrow-heavy case, which showed no clean win when chunked) stay on the
/// zero-overhead single-pass demux. Set the knob to `usize::MAX` to
/// disable chunking entirely (the 3b single-pass behaviour).
#[must_use]
fn demux_chunk() -> usize {
    static CHUNK: OnceLock<usize> = OnceLock::new();
    *CHUNK.get_or_init(|| {
        env::var("AETHER_BLOB_DEMUX_CHUNK")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&k| k >= 1)
            .unwrap_or(8)
    })
}

/// One sealed ring blob's worth of routed mail, scheduled as a single
/// `Drainable` (ADR-0087 Phase 3b). One-shot: `run_cycle` demuxes the
/// mail once and returns [`CycleResult::Idle`]; the popped `Arc` then
/// drops.
pub struct BlobWork {
    /// The blob's mail. `Option` so `run_cycle` (which takes `&self`)
    /// can `take` the owned `Vec` out for the one-shot demux; `Mutex`
    /// only for the `&self` interior-mutability + `Sync` requirement —
    /// a blob is demuxed by exactly one worker, so the lock is
    /// uncontended.
    mails: Mutex<Option<Vec<Mail>>>,
    /// Resolves each mail's recipient (`route_lookup`) and deposits the
    /// non-direct-dispatch mails (`push` → `route_mail`) on the demuxing
    /// worker.
    mailer: Arc<Mailer>,
    /// Where a direct-dispatched recipient that yields mid-drain
    /// ([`CycleResult::Requeue`]) is re-scheduled — the same own-deque /
    /// injector path a normal wake uses.
    sink: WakeSink,
    /// Inline-demux chunk cap K (3c). Set from [`demux_chunk`] at
    /// construction and inherited by spilled sub-blobs, so a blob's whole
    /// chunk-cascade uses one consistent K (and tests can pin it without
    /// the process-global env knob).
    chunk: usize,
}

impl BlobWork {
    /// Wrap a flushed blob's routed mail as a schedulable unit. The
    /// chunk cap is read from [`demux_chunk`] (`AETHER_BLOB_DEMUX_CHUNK`).
    pub fn new(mails: Vec<Mail>, mailer: Arc<Mailer>, sink: WakeSink) -> Arc<Self> {
        Self::with_chunk(mails, mailer, sink, demux_chunk())
    }

    /// Construct with an explicit chunk cap — the production path goes
    /// through [`Self::new`] (which reads the env knob); `run_cycle`
    /// threads `self.chunk` into the spilled sub-blob, and tests pin a
    /// small `K` to exercise the spill without the process-global knob.
    fn with_chunk(
        mails: Vec<Mail>,
        mailer: Arc<Mailer>,
        sink: WakeSink,
        chunk: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            mails: Mutex::new(Some(mails)),
            mailer,
            sink,
            chunk,
        })
    }
}

impl Drainable for BlobWork {
    fn run_cycle(&self, budget: BatchBudget) -> CycleResult {
        let Some(mut mails) = self
            .mails
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            // Already demuxed (a blob is one-shot). Defensive — the
            // scheduler never runs a slot twice without a re-push.
            return CycleResult::Idle;
        };

        // ADR-0087 Phase 3c: cap the inline demux at K mails and spill the
        // remainder as a stealable sub-blob, so a wide / heavy fan-out
        // parallelises across idle workers instead of serialising here.
        // `split_off` **moves** the tail `Mail` handles into the new blob
        // — each carries its `MailRef` (an `Arc<MailRing>` + offsets, or an
        // owned box), so the payload bytes are never copied; the sub-blob
        // references the same ring regions and the per-blob reclaim lock
        // counts travel with the moved refs. Spill *before* demuxing our
        // chunk so an idle sibling can steal the remainder off our deque
        // tail while we work; if none is idle we pop it ourselves next loop
        // (LIFO) and continue — bounded work-before-steal-point is K.
        let k = self.chunk;
        if mails.len() > k {
            let rest = mails.split_off(k);
            self.sink.schedule(Self::with_chunk(
                rest,
                Arc::clone(&self.mailer),
                self.sink.clone(),
                k,
            ));
        }

        // ADR-0087 §4 (iamacoffeepot/aether#1135): walk the blob's mail in
        // **send order** and dispatch each in place where we can.
        //
        // Per mail:
        // - Resolve the recipient's seize handle + ref-schema under one
        //   registry read (`route_lookup`).
        // - **Direct-dispatch** when (1) the recipient exposes a seize
        //   handle (a `Pooled` actor's slot), (2) the kind is ref-free
        //   (ADR-0045 ref kinds need `route_mail`'s handle walk / park),
        //   and (3) we win the `Idle → Running` seize CAS: build the
        //   envelope and run the full per-envelope wrapper in place
        //   (`Drainable::seize_and_run` → `dispatch_one` — `Received` /
        //   `Finished` incl. the #1134 `t_enqueue` / `enqueue_depth`,
        //   `record_finished` settlement bracket, the `log.tail` /
        //   `trace.tail` framework arms), then drain the recipient's inbox
        //   and run the post-empty recheck. No inbox deposit, no
        //   `try_recv` repop — residence ≈ 0.
        // - **Deposit** otherwise (no seize handle / busy slot / ref kind)
        //   via today's `Mailer::push` → `route_mail`: the holder (or a
        //   woken cycle) drains it, per-recipient FIFO preserved by the
        //   inbox's own ordering.
        //
        // Per-recipient FIFO holds by construction: the send-order walk
        // hands at most one seize per recipient before the slot returns to
        // `Idle`, and the busy / deposited path goes through the inbox
        // FIFO. Cross-recipient is async (the ordering spine's contract),
        // so a busy recipient running on its own thread is correct.
        let mailer = &self.mailer;
        for mail in mails {
            let lookup = mailer.registry().route_lookup(mail.kind, mail.recipient);
            // Direct-dispatch only a ref-free kind whose recipient is a
            // `Pooled` slot we win the seize on. ADR-0045 ref kinds fall
            // through to `route_mail` (the handle walk / park); so do
            // non-`Pooled` recipients (no seize handle) and busy slots
            // (lost the `Idle → Running` CAS — `try_seize` → `None`).
            let seized = if lookup.ref_schema.is_some() {
                None
            } else {
                lookup.seize.as_ref().and_then(SeizeHandle::try_seize)
            };
            match seized {
                Some(slot) => {
                    // Build the seed envelope from the held mail. The
                    // recipient slot is `Running` (we won the seize); the
                    // payload `MailRef` moves in directly (an `InRing` ref
                    // stays pinned — the blob holds the region until this
                    // demux drains, and the seed dispatches synchronously
                    // here). `t_enqueue ≈ now` / `enqueue_depth = 0` — no
                    // queue residence (the #1134 measured win).
                    let seed = Envelope {
                        kind: mail.kind,
                        kind_name: lookup.kind_name,
                        origin: None,
                        sender: mail.reply_to,
                        payload: mail.payload,
                        count: mail.count,
                        mail_id: mail.mail_id,
                        root: mail.root,
                        parent_mail: mail.parent_mail,
                        t_enqueue: mailer.now_nanos(),
                        enqueue_depth: 0,
                    };
                    match slot.seize_and_run(seed, budget) {
                        // Budget hit mid-drain — re-schedule the recipient
                        // the same way a normal wake would spill it.
                        CycleResult::Requeue => self.sink.schedule(slot),
                        CycleResult::Idle | CycleResult::Closed => {}
                    }
                }
                // No seize handle, busy slot, or a ref-carrying kind:
                // deposit through the one router and let the holder /
                // woken cycle drain it.
                None => mailer.push(mail),
            }
        }
        CycleResult::Idle
    }

    fn label(&self) -> &'static str {
        "blob"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: a failed lookup/recv is the test signal"
)]
mod tests {
    use super::*;
    use crate::mail::Registry;
    use crate::mail::registry::{InboxHandler, OwnedDispatch};
    use crate::mail::{KindId, MailRef, MailboxId};
    use crate::scheduler::SpinPark;
    use crate::test_util::fresh_substrate;
    use crossbeam_deque::{Injector, Steal};
    use std::iter;
    use std::sync::mpsc;

    /// Pop one spilled sub-blob from the injector (the off-pool spill
    /// target — the test thread isn't a pool worker, so a spill lands
    /// here rather than on a deque).
    fn drain_one(inj: &Injector<Arc<dyn Drainable>>) -> Option<Arc<dyn Drainable>> {
        loop {
            match inj.steal() {
                Steal::Success(s) => return Some(s),
                Steal::Retry => {}
                Steal::Empty => return None,
            }
        }
    }

    /// Register a sink that forwards each delivered mail's first payload
    /// byte onto `tx`, and return its mailbox id.
    fn counting_sink(registry: &Registry, tx: mpsc::Sender<u8>) -> MailboxId {
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: OwnedDispatch| {
            let _ = tx.send(d.payload.bytes()[0]);
        });
        registry.register_inbox("sink", handler);
        registry.lookup("sink").unwrap()
    }

    fn owned_mails(recipient: MailboxId, n: u8) -> Vec<Mail> {
        (0..n)
            .map(|i| Mail::new(recipient, KindId(7), MailRef::from(vec![i]), 1))
            .collect()
    }

    /// 3c: a blob wider than `K` splits the first `K` off to demux inline
    /// and spills the remainder as a sub-blob; running the cascade
    /// delivers every mail exactly once (no loss, no duplication). 5 mails
    /// at K=2 -> chunks [0,1] | [2,3] | [4] = 3 `run_cycle`s.
    #[test]
    fn chunked_spill_delivers_every_mail_once() {
        let (registry, mailer) = fresh_substrate();
        let (tx, rx) = mpsc::channel::<u8>();
        let recipient = counting_sink(&registry, tx);

        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()));
        let blob = BlobWork::with_chunk(owned_mails(recipient, 5), Arc::clone(&mailer), sink, 2);

        let budget = BatchBudget::standard();
        blob.run_cycle(budget);
        let mut chunks_run = 1;
        while let Some(sub) = drain_one(&injector) {
            sub.run_cycle(budget);
            chunks_run += 1;
        }

        assert_eq!(chunks_run, 3, "5 mails at K=2 cascade into 3 chunks");
        let mut got: Vec<u8> = iter::from_fn(|| rx.try_recv().ok()).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![0, 1, 2, 3, 4],
            "every mail delivered exactly once across the chunk cascade"
        );
    }

    /// 3c: at the default K (`usize::MAX`, chunking off) a blob demuxes in
    /// one pass — byte-for-byte the 3b behaviour, no spill.
    #[test]
    fn chunk_off_demuxes_single_pass() {
        let (registry, mailer) = fresh_substrate();
        let (tx, rx) = mpsc::channel::<u8>();
        let recipient = counting_sink(&registry, tx);

        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()));
        let blob = BlobWork::with_chunk(owned_mails(recipient, 5), mailer, sink, usize::MAX);

        blob.run_cycle(BatchBudget::standard());
        assert!(
            matches!(injector.steal(), Steal::Empty),
            "K=MAX never spills a sub-blob"
        );
        assert_eq!(
            iter::from_fn(|| rx.try_recv().ok()).count(),
            5,
            "all mails delivered in the single pass"
        );
    }

    use crate::scheduler::{SeizeSeed, SlotState};
    use aether_data::{KindDescriptor, SchemaCell, SchemaType};

    /// A `Pooled`-shaped recipient fixture for the claim-and-dispatch-
    /// direct demux (iamacoffeepot/aether#1135): it carries a real
    /// [`SlotState`] (so a [`SeizeHandle`] can drive the `Idle → Running`
    /// seize CAS) and records each **direct-dispatched** seed's first
    /// payload byte. [`Drainable::seize_and_run`] is the only arm a blob
    /// demux reaches on this fixture; it stamps the byte and parks the
    /// slot back to `Idle` (mirroring a real slot draining empty), so a
    /// later mail to the same recipient can seize again — the send-order
    /// FIFO property the demux must preserve.
    struct SeizableSink {
        state: Arc<SlotState>,
        direct: mpsc::Sender<u8>,
    }

    impl Drainable for SeizableSink {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }
        fn seize_and_run(&self, seed: SeizeSeed, _budget: BatchBudget) -> CycleResult {
            let _ = self.direct.send(seed.payload.bytes()[0]);
            // Real slots end an empty cycle back in `Idle` via the
            // post-empty recheck; mirror that so the recipient is
            // seizable for the next send-order mail.
            self.state.mark_idle();
            CycleResult::Idle
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Register a closure inbox under `name` (the **deposit** target) and
    /// install a [`SeizeHandle`] over a [`SeizableSink`] fixture (the
    /// **direct** target). Returns the recipient id plus a receiver for
    /// each path so a test can tell which one a mail took. The fixture's
    /// strong `Arc` is returned so the seize handle's `Weak` upgrades for
    /// the duration of the test.
    fn seizable_recipient(
        registry: &Registry,
        name: &str,
    ) -> (
        MailboxId,
        Arc<SeizableSink>,
        mpsc::Receiver<u8>,
        mpsc::Receiver<u8>,
    ) {
        let (deposit_tx, deposit_rx) = mpsc::channel::<u8>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: OwnedDispatch| {
            let _ = deposit_tx.send(d.payload.bytes()[0]);
        });
        let id = registry.register_inbox(name, handler);

        let (direct_tx, direct_rx) = mpsc::channel::<u8>();
        let fixture = Arc::new(SeizableSink {
            state: Arc::new(SlotState::new()),
            direct: direct_tx,
        });
        let slot_dyn: Arc<dyn Drainable> = fixture.clone();
        let installed = registry.install_seize_handle(
            id,
            SeizeHandle::new(Arc::clone(&fixture.state), Arc::downgrade(&slot_dyn)),
        );
        assert!(installed, "seize handle installs on a live Inbox entry");
        (id, fixture, direct_rx, deposit_rx)
    }

    /// Free `Pooled` recipient → the demux seizes it and dispatches in
    /// place: the seed lands on the **direct** path, the inbox-deposit
    /// path is never touched.
    #[test]
    fn free_recipient_dispatched_direct_no_inbox_bounce() {
        let (registry, mailer) = fresh_substrate();
        let (recipient, _fixture, direct_rx, deposit_rx) =
            seizable_recipient(&registry, "seizable");

        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()));
        let blob = BlobWork::with_chunk(owned_mails(recipient, 1), mailer, sink, usize::MAX);

        blob.run_cycle(BatchBudget::standard());

        assert_eq!(
            direct_rx.try_recv().ok(),
            Some(0),
            "seed dispatched in place"
        );
        assert!(
            deposit_rx.try_recv().is_err(),
            "a direct-dispatched mail never bounces through the inbox"
        );
    }

    /// Busy `Pooled` recipient (slot already `Running`) → the seize loses
    /// the CAS, so the mail is **deposited** through `route_mail`, not
    /// dispatched in place.
    #[test]
    fn busy_recipient_deposited() {
        let (registry, mailer) = fresh_substrate();
        let (recipient, fixture, direct_rx, deposit_rx) = seizable_recipient(&registry, "seizable");

        // Mark the recipient busy before the demux: its `Idle → Running`
        // seize must lose, falling through to the deposit path.
        assert!(
            fixture.state.seize(),
            "fixture starts Idle, seize wins once"
        );

        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()));
        let blob = BlobWork::with_chunk(owned_mails(recipient, 1), mailer, sink, usize::MAX);

        blob.run_cycle(BatchBudget::standard());

        assert_eq!(
            deposit_rx.try_recv().ok(),
            Some(0),
            "a busy recipient's mail is deposited"
        );
        assert!(
            direct_rx.try_recv().is_err(),
            "a busy recipient is not dispatched in place"
        );
    }

    /// ADR-0045 ref-carrying kind → never direct-dispatched even to a free
    /// `Pooled` recipient: `route_mail` owns the handle walk / park, so the
    /// mail is **deposited**.
    #[test]
    fn ref_kind_deposited() {
        let (registry, mailer) = fresh_substrate();
        let (recipient, _fixture, direct_rx, deposit_rx) =
            seizable_recipient(&registry, "seizable");

        // A kind whose schema embeds a `Ref` → `route_lookup.ref_schema`
        // is `Some`, so the demux falls through to deposit.
        let ref_kind = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "test.blob.ref_kind".to_owned(),
                schema: SchemaType::Ref(SchemaCell::owned(SchemaType::Bytes)),
            })
            .expect("fresh ref kind registers");

        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()));
        // Inline-form `Ref` payload: discriminant 0 (inline) + the inner
        // `Bytes` value (postcard `varint(len=0)` → empty). The ref-walk
        // resolves it inline and the recipient's deposit handler receives
        // the resolved bytes (first byte `0` — the inline discriminant
        // the closure reads). The test asserts the routing *decision*: a
        // ref kind is deposited (so `route_mail` owns the walk), never
        // direct-dispatched.
        let mails = vec![Mail::new(
            recipient,
            ref_kind,
            MailRef::from(vec![0u8, 0u8]),
            1,
        )];
        let blob = BlobWork::with_chunk(mails, mailer, sink, usize::MAX);

        blob.run_cycle(BatchBudget::standard());

        assert!(
            direct_rx.try_recv().is_err(),
            "a ref-carrying kind is never dispatched in place"
        );
        assert_eq!(
            deposit_rx.try_recv().ok(),
            Some(0),
            "a ref-carrying kind is deposited so route_mail can walk it"
        );
    }

    /// Two mails to one free recipient → both dispatch in place, in send
    /// order (per-recipient FIFO). The fixture parks `Idle` after each
    /// seed, so the second mail re-seizes the same slot.
    #[test]
    fn two_seeds_to_one_recipient_run_in_send_order() {
        let (registry, mailer) = fresh_substrate();
        let (recipient, _fixture, direct_rx, deposit_rx) =
            seizable_recipient(&registry, "seizable");

        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()));
        // Two mails (bytes 0, 1) to the same recipient, in send order.
        let blob = BlobWork::with_chunk(owned_mails(recipient, 2), mailer, sink, usize::MAX);

        blob.run_cycle(BatchBudget::standard());

        assert_eq!(direct_rx.try_recv().ok(), Some(0), "first seed first");
        assert_eq!(direct_rx.try_recv().ok(), Some(1), "second seed second");
        assert!(direct_rx.try_recv().is_err(), "exactly two seeds");
        assert!(
            deposit_rx.try_recv().is_err(),
            "both mails dispatched in place — no inbox deposit"
        );
    }

    /// A closure-backed inbox (no slot) exposes no seize handle, so its
    /// mail is deposited — the path the legacy `counting_sink` tests
    /// already exercise, asserted here against the seize-resolution.
    #[test]
    fn closure_inbox_has_no_seize_handle() {
        let (registry, _mailer) = fresh_substrate();
        let (tx, _rx) = mpsc::channel::<u8>();
        let recipient = counting_sink(&registry, tx);
        let lookup = registry.route_lookup(KindId(7), recipient);
        assert!(
            lookup.seize.is_none(),
            "a closure-backed inbox has no slot to seize"
        );
    }
}
