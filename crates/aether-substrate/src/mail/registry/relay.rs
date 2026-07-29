use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use super::mailbox::RouteContinuation;
use super::metrics::{INITIAL_QUEUE_RESERVE, QueueMeter, RegistryQueueMetrics};
use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState, WakeHandle, WakeSink};

#[derive(Clone)]
pub struct RouteRelayHandle {
    state: Arc<Mutex<RelayQueue>>,
    meter: Arc<QueueMeter>,
    wake: WakeHandle,
}

struct RelayQueue {
    accepting: bool,
    /// Pressure bound in continuations. The relay carries no sheddable class
    /// — see [`RouteRelayHandle::submit`] — so crossing this is counted and
    /// warned, never refused.
    capacity: usize,
    /// Latched for the duration of one over-capacity episode so the warn
    /// fires once per episode rather than once per continuation. Cleared by
    /// the drain that empties the queue.
    saturated: bool,
    continuations: VecDeque<RouteContinuation>,
}

impl RouteRelayHandle {
    /// Hand a continuation to the relay's serialization, returning it
    /// unqueued only when the relay has stopped accepting.
    ///
    /// The relay does not shed (issue 4122). Everything reaching it is a
    /// continuation the registry owner already decided the fate of —
    /// principally the parked FIFO released at `Live` publication, whose
    /// order and completeness ADR-0165 §"Pending mail and birth ordering"
    /// makes a contract. Dropping one loses mail the registry committed to
    /// delivering and leaves its settlement chain open, which converts a
    /// memory problem into a correctness problem; and because relay inflow is
    /// exactly owner outflow, the owner's own bound is what limits it. So the
    /// capacity here is the declared pressure line: crossing it increments
    /// `over_capacity` and warns once, which is the observation that says the
    /// relay — not the owner — is the drainer falling behind.
    pub(crate) fn submit(&self, continuation: RouteContinuation) -> Option<RouteContinuation> {
        let mut state = self.state.lock().expect("route relay queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            return Some(continuation);
        }
        state.continuations.push_back(continuation);
        self.meter.admit(state.continuations.len());
        if state.continuations.len() > state.capacity && !state.saturated {
            state.saturated = true;
            tracing::warn!(
                target: "aether_substrate::registry",
                capacity = state.capacity,
                "route relay queue past capacity — owner-committed continuations are not sheddable, admitting anyway",
            );
        }
        drop(state);
        let _ = self.wake.wake();
        None
    }

    pub(crate) fn metrics(&self) -> RegistryQueueMetrics {
        self.meter.snapshot()
    }
}

pub struct RouteRelayLease {
    slot: Arc<RouteRelaySlot>,
    mailer: Arc<Mailer>,
}

impl RouteRelayLease {
    /// Attach the relay slot with an explicit pressure bound. `capacities` is
    /// the whole resolved knob so the relay and its sibling owner read one
    /// value rather than two loose numbers; the relay takes
    /// [`RegistryQueueCapacities::relay`]. A configured `0` clamps to one
    /// continuation.
    pub(crate) fn attach(mailer: &Arc<Mailer>, sink: WakeSink, capacities: RegistryQueueCapacities) -> Self {
        let capacity = capacities.relay.max(1);
        let state = Arc::new(Mutex::new(RelayQueue {
            accepting: true,
            capacity,
            saturated: false,
            continuations: VecDeque::with_capacity(capacity.min(INITIAL_QUEUE_RESERVE)),
        }));
        let meter = Arc::new(QueueMeter::new(capacity));
        let slot = Arc::new(RouteRelaySlot {
            mailer: Arc::downgrade(mailer),
            queue: Arc::clone(&state),
            meter: Arc::clone(&meter),
            route_lock: Mutex::new(()),
            state: Arc::new(SlotState::new()),
        });
        let erased: Arc<dyn Drainable> = slot.clone();
        let wake = WakeHandle::new(Arc::clone(&slot.state), Arc::downgrade(&erased), sink);
        mailer.install_route_relay(RouteRelayHandle { state, meter, wake });
        Self { slot, mailer: Arc::clone(mailer) }
    }

    #[cfg(test)]
    pub(super) fn run_once(&self) -> CycleResult {
        self.slot.run_cycle(BatchBudget::standard())
    }

    #[cfg(test)]
    pub(super) fn drainable_for_test(&self) -> Arc<dyn Drainable> {
        self.slot.clone()
    }

    #[cfg(test)]
    pub(super) fn route_serialization_held_for_test(&self) -> bool {
        self.slot.route_lock.try_lock().is_err()
    }
}

impl Drop for RouteRelayLease {
    fn drop(&mut self) {
        let started = Instant::now();
        let _route = self.slot.route_lock.lock().expect("route relay lock poisoned; fail-fast per ADR-0063");
        let continuations = {
            let mut state = self.slot.queue.lock().expect("route relay queue lock poisoned; fail-fast per ADR-0063");
            state.accepting = false;
            let continuations = state.continuations.drain(..).collect::<Vec<_>>();
            self.slot.meter.drained_to_empty();
            drop(state);
            continuations
        };
        self.slot.meter.drain(continuations.len(), started.elapsed());
        for continuation in continuations {
            self.mailer.route_captured(continuation);
        }
    }
}

struct RouteRelaySlot {
    mailer: Weak<Mailer>,
    queue: Arc<Mutex<RelayQueue>>,
    meter: Arc<QueueMeter>,
    route_lock: Mutex<()>,
    state: Arc<SlotState>,
}

impl Drainable for RouteRelaySlot {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if !self.state.enter_running() {
            return CycleResult::Idle;
        }

        {
            let started = Instant::now();
            let _route = self.route_lock.lock().expect("route relay lock poisoned; fail-fast per ADR-0063");
            let continuations = {
                let mut queue = self.queue.lock().expect("route relay queue lock poisoned; fail-fast per ADR-0063");
                queue.saturated = false;
                let continuations = queue.continuations.drain(..).collect::<Vec<_>>();
                self.meter.drained_to_empty();
                drop(queue);
                continuations
            };
            let drained = continuations.len();
            if !continuations.is_empty() {
                let mailer = self.mailer.upgrade().expect(
                    "accepted route continuations outlived the relay lease's Mailer; drain serialization is broken",
                );
                for continuation in continuations {
                    mailer.route_captured(continuation);
                }
            }
            self.meter.drain(drained, started.elapsed());
        }

        self.state.mark_idle();
        if self.queue.lock().expect("route relay queue lock poisoned; fail-fast per ADR-0063").continuations.is_empty()
        {
            CycleResult::Idle
        } else if self.state.try_self_requeue() {
            CycleResult::Requeue
        } else {
            CycleResult::Idle
        }
    }

    fn label(&self) -> &'static str {
        "registry-route-relay"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
