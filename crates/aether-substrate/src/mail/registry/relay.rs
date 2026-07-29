use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use super::mailbox::RouteContinuation;
use crate::mail::mailer::Mailer;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState, WakeHandle, WakeSink};

#[derive(Clone)]
pub struct RouteRelayHandle {
    state: Arc<Mutex<RelayQueue>>,
    wake: WakeHandle,
}

struct RelayQueue {
    accepting: bool,
    continuations: VecDeque<RouteContinuation>,
}

impl RouteRelayHandle {
    pub(crate) fn submit(&self, continuation: RouteContinuation) -> Option<RouteContinuation> {
        let mut state = self.state.lock().expect("route relay queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            return Some(continuation);
        }
        state.continuations.push_back(continuation);
        drop(state);
        let _ = self.wake.wake();
        None
    }
}

pub struct RouteRelayLease {
    slot: Arc<RouteRelaySlot>,
    mailer: Arc<Mailer>,
}

impl RouteRelayLease {
    pub(crate) fn attach(mailer: &Arc<Mailer>, sink: WakeSink) -> Self {
        let state = Arc::new(Mutex::new(RelayQueue { accepting: true, continuations: VecDeque::new() }));
        let slot = Arc::new(RouteRelaySlot {
            mailer: Arc::downgrade(mailer),
            queue: Arc::clone(&state),
            route_lock: Mutex::new(()),
            state: Arc::new(SlotState::new()),
        });
        let erased: Arc<dyn Drainable> = slot.clone();
        let wake = WakeHandle::new(Arc::clone(&slot.state), Arc::downgrade(&erased), sink);
        mailer.install_route_relay(RouteRelayHandle { state, wake });
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
        let _route = self.slot.route_lock.lock().expect("route relay lock poisoned; fail-fast per ADR-0063");
        let continuations = {
            let mut state = self.slot.queue.lock().expect("route relay queue lock poisoned; fail-fast per ADR-0063");
            state.accepting = false;
            state.continuations.drain(..).collect::<Vec<_>>()
        };
        for continuation in continuations {
            self.mailer.route_captured(continuation);
        }
    }
}

struct RouteRelaySlot {
    mailer: Weak<Mailer>,
    queue: Arc<Mutex<RelayQueue>>,
    route_lock: Mutex<()>,
    state: Arc<SlotState>,
}

impl Drainable for RouteRelaySlot {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if !self.state.enter_running() {
            return CycleResult::Idle;
        }

        {
            let _route = self.route_lock.lock().expect("route relay lock poisoned; fail-fast per ADR-0063");
            let continuations = self
                .queue
                .lock()
                .expect("route relay queue lock poisoned; fail-fast per ADR-0063")
                .continuations
                .drain(..)
                .collect::<Vec<_>>();
            if !continuations.is_empty() {
                let mailer = self.mailer.upgrade().expect(
                    "accepted route continuations outlived the relay lease's Mailer; drain serialization is broken",
                );
                for continuation in continuations {
                    mailer.route_captured(continuation);
                }
            }
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
