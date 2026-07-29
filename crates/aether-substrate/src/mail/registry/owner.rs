use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use crossbeam_channel::Sender;

use super::effect::{EffectBatch, RegistryApplied, RegistryCompletion, RegistryEffectError};
use super::mailbox::Registry;
use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState, WakeHandle, WakeSink};

pub(super) struct BatchEnvelope {
    pub(super) batch: EffectBatch,
    pub(super) completion: Sender<Result<Vec<RegistryApplied>, RegistryEffectError>>,
}

pub(super) enum OwnerCommand {
    Batch(BatchEnvelope),
    ParkOrDrop(Mail),
}

#[derive(Clone)]
pub(super) struct RegistryOwnerHandle {
    state: Arc<Mutex<OwnerQueue>>,
    wake: WakeHandle,
}

struct OwnerQueue {
    accepting: bool,
    commands: VecDeque<OwnerCommand>,
}

impl RegistryOwnerHandle {
    pub(super) fn submit(&self, batch: EffectBatch) -> Option<RegistryCompletion<Vec<RegistryApplied>>> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let mut state = self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            return None;
        }
        state.commands.push_back(OwnerCommand::Batch(BatchEnvelope { batch, completion: sender }));
        drop(state);
        let _ = self.wake.wake();
        Some(RegistryCompletion::new(receiver))
    }

    pub(super) fn park_or_drop(&self, mail: Mail) -> Option<Mail> {
        let mut state = self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            return Some(mail);
        }
        state.commands.push_back(OwnerCommand::ParkOrDrop(mail));
        drop(state);
        let _ = self.wake.wake();
        None
    }

    pub(super) fn is_accepting(&self) -> bool {
        self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063").accepting
    }
}

pub struct RegistryOwnerLease {
    slot: Arc<RegistryOwnerSlot>,
}

impl RegistryOwnerLease {
    pub fn attach(registry: &Arc<Registry>, mailer: &Arc<Mailer>, sink: WakeSink) -> Self {
        let state = Arc::new(Mutex::new(OwnerQueue { accepting: true, commands: VecDeque::new() }));
        let slot = Arc::new(RegistryOwnerSlot {
            registry: Arc::downgrade(registry),
            mailer: Arc::clone(mailer),
            queue: Arc::clone(&state),
            apply_lock: Mutex::new(()),
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
        let _apply = self.slot.apply_lock.lock().expect("registry owner apply lock poisoned; fail-fast per ADR-0063");
        let queued = {
            let mut state = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
            state.accepting = false;
            state.commands.drain(..).collect::<Vec<_>>()
        };
        if let Some(registry) = self.slot.registry.upgrade() {
            registry.close_owner_commands(queued, &self.slot.mailer);
        } else {
            for command in queued {
                if let OwnerCommand::Batch(envelope) = command {
                    let _ = envelope.completion.send(Err(RegistryEffectError::OwnerClosed));
                }
            }
        }
    }
}

struct RegistryOwnerSlot {
    registry: Weak<Registry>,
    mailer: Arc<Mailer>,
    queue: Arc<Mutex<OwnerQueue>>,
    apply_lock: Mutex<()>,
    state: Arc<SlotState>,
}

impl Drainable for RegistryOwnerSlot {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if !self.state.enter_running() {
            return CycleResult::Idle;
        }

        {
            let _apply = self.apply_lock.lock().expect("registry owner apply lock poisoned; fail-fast per ADR-0063");
            let (commands, accepting) = {
                let mut queue = self.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
                (queue.commands.drain(..).collect::<Vec<_>>(), queue.accepting)
            };

            if !commands.is_empty() {
                if let Some(registry) = self.registry.upgrade() {
                    if accepting {
                        registry.apply_owner_commands(commands, &self.mailer);
                    } else {
                        registry.close_owner_commands(commands, &self.mailer);
                    }
                } else {
                    for command in commands {
                        if let OwnerCommand::Batch(envelope) = command {
                            let _ = envelope.completion.send(Err(RegistryEffectError::OwnerClosed));
                        }
                    }
                }
            }
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
