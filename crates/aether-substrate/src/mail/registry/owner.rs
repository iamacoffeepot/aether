use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use crossbeam_channel::Sender;

use super::effect::{EffectBatch, RegistryApplied, RegistryCompletion, RegistryEffectError};
use super::mailbox::Registry;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState, WakeHandle, WakeSink};

pub(super) struct BatchEnvelope {
    pub(super) batch: EffectBatch,
    pub(super) completion: Sender<Result<Vec<RegistryApplied>, RegistryEffectError>>,
}

#[derive(Clone)]
pub(super) struct RegistryOwnerHandle {
    state: Arc<Mutex<OwnerQueue>>,
    wake: WakeHandle,
}

struct OwnerQueue {
    accepting: bool,
    envelopes: VecDeque<BatchEnvelope>,
}

impl RegistryOwnerHandle {
    pub(super) fn submit(&self, batch: EffectBatch) -> Option<RegistryCompletion<Vec<RegistryApplied>>> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let mut state = self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            return None;
        }
        state.envelopes.push_back(BatchEnvelope { batch, completion: sender });
        drop(state);
        let _ = self.wake.wake();
        Some(RegistryCompletion::new(receiver))
    }

    pub(super) fn is_accepting(&self) -> bool {
        self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063").accepting
    }
}

pub struct RegistryOwnerLease {
    slot: Arc<RegistryOwnerSlot>,
}

impl RegistryOwnerLease {
    pub fn attach(registry: &Arc<Registry>, sink: WakeSink) -> Self {
        let state = Arc::new(Mutex::new(OwnerQueue { accepting: true, envelopes: VecDeque::new() }));
        let slot = Arc::new(RegistryOwnerSlot {
            registry: Arc::downgrade(registry),
            queue: Arc::clone(&state),
            state: Arc::new(SlotState::new()),
        });
        let erased: Arc<dyn Drainable> = slot.clone();
        let wake = WakeHandle::new(Arc::clone(&slot.state), Arc::downgrade(&erased), sink);
        let handle = RegistryOwnerHandle { state, wake };
        registry.install_owner(handle);

        Self { slot }
    }

    #[cfg(test)]
    pub(super) fn run_once(&self) -> CycleResult {
        self.slot.run_cycle(BatchBudget::standard())
    }
}

impl Drop for RegistryOwnerLease {
    fn drop(&mut self) {
        let queued = {
            let mut state = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
            state.accepting = false;
            state.envelopes.drain(..).collect::<Vec<_>>()
        };
        for envelope in queued {
            let _ = envelope.completion.send(Err(RegistryEffectError::OwnerClosed));
        }
    }
}

struct RegistryOwnerSlot {
    registry: Weak<Registry>,
    queue: Arc<Mutex<OwnerQueue>>,
    state: Arc<SlotState>,
}

impl Drainable for RegistryOwnerSlot {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if !self.state.enter_running() {
            return CycleResult::Idle;
        }

        let envelopes = self
            .queue
            .lock()
            .expect("registry owner queue lock poisoned; fail-fast per ADR-0063")
            .envelopes
            .drain(..)
            .collect::<Vec<_>>();

        if !envelopes.is_empty() {
            if let Some(registry) = self.registry.upgrade() {
                registry.apply_owner_envelopes(envelopes);
            } else {
                for envelope in envelopes {
                    let _ = envelope.completion.send(Err(RegistryEffectError::OwnerClosed));
                }
            }
        }

        self.state.mark_idle();
        if self.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063").envelopes.is_empty() {
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
