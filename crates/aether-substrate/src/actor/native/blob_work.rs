//! [`BlobWork`] — a handler's buffered fan-out as a single
//! work-stealing unit (ADR-0087 Phase 3b, iamacoffeepot/aether#1113).
//!
//! Phase 2b/2c buffer a native handler's outbound mail into one sealed
//! ring blob; Phase 3a made per-worker deques the scheduler's queue.
//! 3b ties them together: instead of routing each buffered mail
//! eagerly (N pushes + up to N parked-worker wakeups for a fan-out of
//! N), [`crate::actor::native::NativeBinding::flush_outbound`] pushes
//! the whole blob as **one** `Drainable` onto the producing worker's
//! deque. A worker pops it (or an idle sibling steals it) and demuxes:
//!
//! - **free** recipient (won the `Idle → Ready` CAS) → run its handler
//!   **inline** on this worker — no inbox round-trip beyond the deposit,
//!   no notify.
//! - **busy** recipient (lost the CAS) → its mail is already deposited;
//!   the holder draining it picks it up (today's wake-loses-CAS path).
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
//! `run_cycle` deposits via today's [`Mailer::push`] → `route_mail` for
//! every mail, so **all** routing concerns are inherited unchanged:
//! ADR-0045 ref-walk, the settlement/trace brackets (the inline run
//! goes through `DispatcherSlot::run_cycle` → `dispatch_one`, which owns
//! `Received`/`Finished` + `record_finished`), and the
//! `Inline`/`Dropped`/unknown-bubble-up arms. The *only* thing 3b
//! changes is the wake's destination: inside the [`run_demux`] window, a
//! free recipient's wake is collected (not deque-pushed) so we can run it
//! inline here.
//!
//! ## Alternative not taken — explicit slot-demux registry (revisit?)
//!
//! The other shape (iamacoffeepot/aether#1113 design fork) was a
//! first-class `MailboxId → (SlotState, Weak<slot>)` table the blob
//! worker resolves directly, depositing via `ActorRegistry::live_sender`
//! and routing only non-`Inbox` recipients through `route_mail`. It is
//! more inspectable, but (1) it splits routing into a fast path + a
//! `route_mail` fallback that must be kept in lockstep for every future
//! routing concern, and (2) it adds a second shared read-hot map on the
//! hottest path — a re-centralization against the grain of ADR-0086/0087
//! (which removed the central `SegQueue` for per-actor/per-worker
//! structures). The reuse approach here keeps one router and adds no
//! shared map. **Reconsider the explicit table only if the scheduler
//! later needs first-class slot addressing for other consumers**
//! (affinity-as-data, slot introspection, NUMA placement) — at which
//! point its real consumers exist and the table earns its keep.

use std::any::Any;
use std::env;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, WakeSink, run_demux};

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
    /// Routes each mail (deposit + wake) on the demuxing worker.
    mailer: Arc<Mailer>,
    /// Where an inline recipient that yields mid-drain
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

        // Deposit every mail through the one router; harvest the free
        // recipients it woke (those that won the Idle→Ready CAS).
        let mailer = &self.mailer;
        let collected = run_demux(|| {
            for mail in mails {
                mailer.push(mail);
            }
        });

        // Run each free recipient inline on this worker. A recipient
        // that yields mid-drain (budget hit) is re-scheduled the same
        // way a normal wake would spill it; Idle/Closed just drop.
        for slot in collected {
            match slot.run_cycle(budget) {
                CycleResult::Requeue => self.sink.schedule(slot),
                CycleResult::Idle | CycleResult::Closed => {}
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
}
