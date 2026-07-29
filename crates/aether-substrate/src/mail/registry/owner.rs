use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
#[cfg(test)]
use std::thread;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use super::effect::{
    ACTIVATION_BARRIER_KIND, ActivationToken, EffectBatch, RegistryApplied, RegistryBatchCompletionSink,
    RegistryBatchResult, RegistryCompletion, RegistryEffectError,
};
use super::mailbox::Registry;
use super::metrics::{INITIAL_QUEUE_RESERVE, QueueMeter, RegistryQueueMetrics};
use crate::actor::native::dispatch_blocking::DeferredCompletion;
use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::{Mail, MailboxId};
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState, WakeHandle, WakeSink};

pub(super) struct BatchEnvelope {
    pub(super) batch: EffectBatch,
    pub(super) completion: RegistryBatchCompletionSink,
}

pub(super) enum OwnerCommand {
    Batch(BatchEnvelope),
    ParkOrDrop { mail: Mail, observed_generation: u64 },
    ActivationCancelled { id: MailboxId, token: ActivationToken },
}

impl OwnerCommand {
    /// Whether the bound may refuse this command (issue 4122).
    ///
    /// Only an ordinary route-view miss is sheddable. Its volume is set by
    /// whoever is addressing mail — a buggy or hostile sender spraying
    /// nonexistent recipients turns wrong addressing into owner work — and
    /// shedding one applies the same terminal treatment the owner would give
    /// an absent recipient anyway (ADR-0165 §"Pending mail and birth
    /// ordering": `absent -> apply the existing unknown-recipient policy`),
    /// so nothing downstream can tell the two apart.
    ///
    /// Everything else is reserved. An effect batch carries prepared registry
    /// state whose loss is a correctness failure, not a delayed delivery; an
    /// `ActivationCancelled` releases a reservation, and losing it strands a
    /// `Starting` route and every envelope parked behind it; an activation
    /// barrier is the control envelope that promotes a birth to `Live`.
    /// Their volume is bounded by real engine work — one batch per handler
    /// flush, one barrier per birth — so admitting them past the bound cannot
    /// be driven from outside. `over_capacity` counts when it happens.
    fn sheddable(&self) -> bool {
        match self {
            Self::ParkOrDrop { mail, .. } => mail.kind != ACTIVATION_BARRIER_KIND,
            Self::Batch(_) | Self::ActivationCancelled { .. } => false,
        }
    }
}

pub enum ParkAdmission {
    Queued,
    Retry(Mail),
    Closed(Mail),
    /// The owner queue stood at its bound and this envelope was sheddable.
    /// The caller applies the existing unknown-recipient policy to the
    /// returned `Mail`, which is what the owner would have applied to an
    /// absent recipient.
    ///
    /// Shedding removes an envelope; it never moves one. The parked FIFO
    /// still receives what it receives in owner-observed order, so ADR-0165's
    /// birth-ordering contract holds — a shed is a loss under saturation, not
    /// a reordering.
    Shed(Mail),
}

#[derive(Clone)]
pub(super) struct RegistryOwnerHandle {
    state: Arc<Mutex<OwnerQueue>>,
    meter: Arc<QueueMeter>,
    wake: WakeHandle,
}

struct OwnerQueue {
    accepting: bool,
    /// Admission bound in commands. Refuses the sheddable class only; see
    /// [`OwnerCommand::sheddable`].
    capacity: usize,
    /// Whether the queue is inside a saturation episode. Latches on the first
    /// shed so the warn fires once per episode instead of once per shed
    /// envelope — a spraying sender must not be able to turn a shed policy
    /// into a log storm. Cleared by the drain that empties the queue.
    saturated: bool,
    route_generation: u64,
    commands: VecDeque<OwnerCommand>,
}

impl OwnerQueue {
    /// Admit `command` unless the bound refuses it, in which case it is
    /// handed back for the caller to shed. Records the outcome on `meter`.
    fn admit(&mut self, meter: &QueueMeter, command: OwnerCommand) -> Option<OwnerCommand> {
        if command.sheddable() && self.commands.len() >= self.capacity {
            meter.shed();
            if !self.saturated {
                self.saturated = true;
                tracing::warn!(
                    target: "aether_substrate::registry",
                    capacity = self.capacity,
                    "registry owner queue at capacity — shedding route-view misses to the unknown-recipient policy",
                );
            }
            return Some(command);
        }
        self.commands.push_back(command);
        meter.admit(self.commands.len());
        None
    }
}

impl RegistryOwnerHandle {
    pub(super) fn submit(&self, batch: EffectBatch) -> Option<RegistryCompletion<Vec<RegistryApplied>>> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        self.submit_with(batch, RegistryBatchCompletionSink::Channel(sender))?;
        Some(RegistryCompletion::new(receiver))
    }

    pub(super) fn submit_deferred(
        &self,
        batch: EffectBatch,
        completion: DeferredCompletion<RegistryBatchResult>,
    ) -> bool {
        self.submit_with(batch, RegistryBatchCompletionSink::Deferred(completion)).is_some()
    }

    fn submit_with(&self, batch: EffectBatch, completion: RegistryBatchCompletionSink) -> Option<()> {
        let mut state = self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            drop(state);
            drop(batch.discard_prepared());
            completion.complete(Err(RegistryEffectError::OwnerClosed));
            return None;
        }
        let refused = state.admit(&self.meter, OwnerCommand::Batch(BatchEnvelope { batch, completion }));
        debug_assert!(refused.is_none(), "an effect batch is reserved and is never refused by the bound");
        drop(state);
        let _ = self.wake.wake();
        Some(())
    }

    pub(super) fn park_or_drop(&self, mail: Mail, observed_generation: u64) -> ParkAdmission {
        let mut state = self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            return ParkAdmission::Closed(mail);
        }
        if state.route_generation != observed_generation {
            if observed_generation > state.route_generation {
                state.route_generation = observed_generation;
            }
            return ParkAdmission::Retry(mail);
        }
        if let Some(refused) = state.admit(&self.meter, OwnerCommand::ParkOrDrop { mail, observed_generation }) {
            drop(state);
            // A refusal hands back the command it never pushed, and only the
            // `ParkOrDrop` arm is sheddable, so the envelope is intact here.
            let OwnerCommand::ParkOrDrop { mail, .. } = refused else {
                unreachable!("only a ParkOrDrop command is sheddable")
            };
            return ParkAdmission::Shed(mail);
        }
        drop(state);
        let _ = self.wake.wake();
        ParkAdmission::Queued
    }

    pub(super) fn activation_cancelled(&self, id: MailboxId, token: ActivationToken) -> bool {
        let mut state = self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            return false;
        }
        let refused = state.admit(&self.meter, OwnerCommand::ActivationCancelled { id, token });
        debug_assert!(refused.is_none(), "an activation cancellation is reserved and is never refused by the bound");
        drop(state);
        let _ = self.wake.wake();
        true
    }

    pub(super) fn is_accepting(&self) -> bool {
        self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063").accepting
    }

    pub(super) fn metrics(&self) -> RegistryQueueMetrics {
        self.meter.snapshot()
    }
}

pub struct RegistryOwnerLease {
    slot: Arc<RegistryOwnerSlot>,
}

impl RegistryOwnerLease {
    /// Attach the owner slot with an explicit admission bound. `capacities`
    /// is the whole resolved knob so the owner and its sibling relay read one
    /// value rather than two loose numbers; the owner takes
    /// [`RegistryQueueCapacities::owner`]. A configured `0` clamps to one
    /// command so the queue can always hold the item it is about to drain.
    pub fn attach(
        registry: &Arc<Registry>,
        mailer: &Arc<Mailer>,
        sink: WakeSink,
        capacities: RegistryQueueCapacities,
    ) -> Self {
        let capacity = capacities.owner.max(1);
        let state = Arc::new(Mutex::new(OwnerQueue {
            accepting: true,
            capacity,
            saturated: false,
            route_generation: registry.current_route_generation(),
            commands: VecDeque::with_capacity(capacity.min(INITIAL_QUEUE_RESERVE)),
        }));
        let meter = Arc::new(QueueMeter::new(capacity));
        let slot = Arc::new(RegistryOwnerSlot {
            registry: Arc::downgrade(registry),
            mailer: Arc::clone(mailer),
            queue: Arc::clone(&state),
            meter: Arc::clone(&meter),
            apply_lock: Mutex::new(()),
            state: Arc::new(SlotState::new()),
        });
        let erased: Arc<dyn Drainable> = slot.clone();
        let wake = WakeHandle::new(Arc::clone(&slot.state), Arc::downgrade(&erased), sink);
        let handle = RegistryOwnerHandle { state, meter, wake };
        registry.install_owner(handle);

        Self { slot }
    }

    #[cfg(test)]
    pub(crate) fn run_once(&self) -> CycleResult {
        self.slot.run_cycle(BatchBudget::standard())
    }

    #[cfg(test)]
    pub(crate) fn apply_once_then_close_after_next_command(&self) {
        let apply = self.slot.apply_lock.lock().expect("registry owner apply lock poisoned; fail-fast per ADR-0063");
        let registry = self.slot.registry.upgrade().expect("test registry remains live");
        let mut queue = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        let commands = queue.commands.drain(..).collect::<Vec<_>>();
        self.slot.meter.drained_to_empty();
        drop(queue);
        let route_generation = registry.apply_owner_commands(commands, &self.slot.mailer);
        let mut queue = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        queue.route_generation = queue.route_generation.max(route_generation);
        drop(queue);

        let deadline = Instant::now() + Duration::from_secs(1);
        let commands = loop {
            let mut queue = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
            if !queue.commands.is_empty() {
                queue.accepting = false;
                let commands = queue.commands.drain(..).collect::<Vec<_>>();
                self.slot.meter.drained_to_empty();
                break commands;
            }
            drop(queue);
            assert!(Instant::now() < deadline, "runtime activation barrier reached the owner queue");
            thread::yield_now();
        };
        drop(apply);
        registry.close_owner_commands(commands, &self.slot.mailer);
    }
}

impl Drop for RegistryOwnerLease {
    fn drop(&mut self) {
        let started = Instant::now();
        let apply = self.slot.apply_lock.lock().expect("registry owner apply lock poisoned; fail-fast per ADR-0063");
        let queued = {
            let mut state = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
            state.accepting = false;
            let queued = state.commands.drain(..).collect::<Vec<_>>();
            self.slot.meter.drained_to_empty();
            drop(state);
            queued
        };
        self.slot.meter.drain(queued.len(), started.elapsed());
        drop(apply);
        if let Some(registry) = self.slot.registry.upgrade() {
            registry.close_owner_commands(queued, &self.slot.mailer);
        } else {
            close_orphaned_commands(queued);
        }
    }
}

fn close_orphaned_commands(commands: Vec<OwnerCommand>) {
    let mut discarded = Vec::new();
    for command in commands {
        match command {
            OwnerCommand::Batch(envelope) => {
                discarded.extend(envelope.batch.discard_prepared());
                envelope.completion.complete(Err(RegistryEffectError::OwnerClosed));
            }
            OwnerCommand::ParkOrDrop { .. } | OwnerCommand::ActivationCancelled { .. } => {}
        }
    }
    for done in discarded {
        let _ = done.recv();
    }
}

struct RegistryOwnerSlot {
    registry: Weak<Registry>,
    mailer: Arc<Mailer>,
    queue: Arc<Mutex<OwnerQueue>>,
    meter: Arc<QueueMeter>,
    apply_lock: Mutex<()>,
    state: Arc<SlotState>,
}

impl Drainable for RegistryOwnerSlot {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if !self.state.enter_running() {
            return CycleResult::Idle;
        }

        {
            // The drain is measured from the serialization it holds, not just
            // the apply: the owner is one authority, and time spent waiting
            // for its own lock is time it is not retiring commands. This is
            // the `busy_nanos` that ADR-0165's 5%-of-ceiling sharding trigger
            // divides by.
            let started = Instant::now();
            let _apply = self.apply_lock.lock().expect("registry owner apply lock poisoned; fail-fast per ADR-0063");
            // Completion may synchronously route a task wake back to a
            // Starting actor, which re-enters `park_or_drop`. Drain under the
            // admission lock, then release it before apply/completion; the
            // owner remains serialized by `apply_lock`.
            let (commands, accepting) = {
                let mut queue = self.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
                // Draining to empty ends any saturation episode, so the next
                // one warns again rather than staying silent behind a stale
                // latch.
                queue.saturated = false;
                let commands = queue.commands.drain(..).collect::<Vec<_>>();
                self.meter.drained_to_empty();
                (commands, queue.accepting)
            };
            let drained = commands.len();
            let mut orphaned = None;
            let mut route_generation = None;

            if !commands.is_empty() {
                if let Some(registry) = self.registry.upgrade() {
                    route_generation = Some(if accepting {
                        registry.apply_owner_commands(commands, &self.mailer)
                    } else {
                        registry.close_owner_commands(commands, &self.mailer)
                    });
                } else {
                    orphaned = Some(commands);
                }
            }
            if let Some(route_generation) = route_generation {
                let mut queue = self.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
                // A submitter may already have observed a newer published
                // generation while apply completed; never move its retry
                // watermark backwards.
                queue.route_generation = queue.route_generation.max(route_generation);
            }
            if let Some(commands) = orphaned {
                close_orphaned_commands(commands);
            }
            self.meter.drain(drained, started.elapsed());
        }

        self.state.mark_idle();
        if self.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063").commands.is_empty() {
            CycleResult::Idle
        } else if self.state.try_self_requeue() {
            CycleResult::Requeue
        } else {
            // A submitter won Idle -> Ready and scheduled the slot.
            CycleResult::Idle
        }
    }

    fn label(&self) -> &'static str {
        "registry-owner"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
