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
use std::sync::{Arc, Mutex, PoisonError};

use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, WakeSink, run_demux};

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
}

impl BlobWork {
    /// Wrap a flushed blob's routed mail as a schedulable unit.
    pub fn new(mails: Vec<Mail>, mailer: Arc<Mailer>, sink: WakeSink) -> Arc<Self> {
        Arc::new(Self {
            mails: Mutex::new(Some(mails)),
            mailer,
            sink,
        })
    }
}

impl Drainable for BlobWork {
    fn run_cycle(&self, budget: BatchBudget) -> CycleResult {
        let Some(mails) = self
            .mails
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            // Already demuxed (a blob is one-shot). Defensive — the
            // scheduler never runs a slot twice without a re-push.
            return CycleResult::Idle;
        };

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
