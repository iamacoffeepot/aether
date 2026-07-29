use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
#[cfg(test)]
use std::thread;
#[cfg(test)]
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use super::effect::{ActivationToken, EffectBatch, RegistryApplied, RegistryCompletion, RegistryEffectError};
use super::mailbox::Registry;
use crate::mail::mailer::Mailer;
use crate::mail::{Mail, MailboxId};
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState, WakeHandle, WakeSink};

pub(super) struct BatchEnvelope {
    pub(super) batch: EffectBatch,
    pub(super) completion: Sender<Result<Vec<RegistryApplied>, RegistryEffectError>>,
}

pub(super) enum OwnerCommand {
    Batch(BatchEnvelope),
    ParkOrDrop { mail: Mail, observed_generation: u64 },
    ActivationCancelled { id: MailboxId, token: ActivationToken },
}

pub enum ParkAdmission {
    Queued,
    Retry(Mail),
    Closed(Mail),
}

#[derive(Clone)]
pub(super) struct RegistryOwnerHandle {
    state: Arc<Mutex<OwnerQueue>>,
    wake: WakeHandle,
}

struct OwnerQueue {
    accepting: bool,
    route_generation: u64,
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
        state.commands.push_back(OwnerCommand::ParkOrDrop { mail, observed_generation });
        drop(state);
        let _ = self.wake.wake();
        ParkAdmission::Queued
    }

    pub(super) fn activation_cancelled(&self, id: MailboxId, token: ActivationToken) -> bool {
        let mut state = self.state.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        if !state.accepting {
            return false;
        }
        state.commands.push_back(OwnerCommand::ActivationCancelled { id, token });
        drop(state);
        let _ = self.wake.wake();
        true
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
        let state = Arc::new(Mutex::new(OwnerQueue {
            accepting: true,
            route_generation: registry.current_route_generation(),
            commands: VecDeque::new(),
        }));
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
    pub(crate) fn run_once(&self) -> CycleResult {
        self.slot.run_cycle(BatchBudget::standard())
    }

    #[cfg(test)]
    pub(crate) fn apply_once_then_close_after_next_command(&self) {
        let apply = self.slot.apply_lock.lock().expect("registry owner apply lock poisoned; fail-fast per ADR-0063");
        let registry = self.slot.registry.upgrade().expect("test registry remains live");
        let mut queue = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
        let commands = queue.commands.drain(..).collect::<Vec<_>>();
        queue.route_generation = registry.apply_owner_commands(commands, &self.slot.mailer);
        drop(queue);

        let deadline = Instant::now() + Duration::from_secs(1);
        let commands = loop {
            let mut queue = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
            if !queue.commands.is_empty() {
                queue.accepting = false;
                break queue.commands.drain(..).collect::<Vec<_>>();
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
        let apply = self.slot.apply_lock.lock().expect("registry owner apply lock poisoned; fail-fast per ADR-0063");
        let queued = {
            let mut state = self.slot.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
            state.accepting = false;
            state.commands.drain(..).collect::<Vec<_>>()
        };
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
                let _ = envelope.completion.send(Err(RegistryEffectError::OwnerClosed));
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
            let mut queue = self.queue.lock().expect("registry owner queue lock poisoned; fail-fast per ADR-0063");
            let commands = queue.commands.drain(..).collect::<Vec<_>>();
            let accepting = queue.accepting;
            let mut orphaned = None;

            if !commands.is_empty() {
                if let Some(registry) = self.registry.upgrade() {
                    if accepting {
                        queue.route_generation = registry.apply_owner_commands(commands, &self.mailer);
                    } else {
                        queue.route_generation = registry.close_owner_commands(commands, &self.mailer);
                    }
                } else {
                    orphaned = Some(commands);
                }
            }
            drop(queue);
            if let Some(commands) = orphaned {
                close_orphaned_commands(commands);
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
