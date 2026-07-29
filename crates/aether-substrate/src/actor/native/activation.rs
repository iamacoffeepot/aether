//! Private legacy adapter for ADR-0165 staged actor activation.

use std::any::{Any, TypeId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use aether_actor::local::ActorSlots;

use super::binding::NativeBinding;
use super::dispatcher_slot::DispatcherSlot;
use super::{Envelope, NativeActor};
use crate::actor::registry::ActorRegistry;
use crate::chassis::ctx::{MailboxWakeSlot, RelayOutcome, relay_or_transfer};
use crate::mail::registry::effect::{
    ACTIVATION_BARRIER_KIND, ActivationReservation, ActivationToken, InstalledActivation, LiveActivation, PreparedMail,
    PreparedSpawnActivation,
};
use crate::mail::registry::{MailboxEntry, OwnedDispatch, Registry, SeizeCell};
use crate::mail::{MailId, MailboxId};
use crate::scheduler::pending_depth;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SeizeHandle, WakeHandle};
use aether_kinds::trace::Nanos;

use super::spawn::Spawner;

pub(super) struct LegacyPreparedActivation<A: NativeActor> {
    spawner: Arc<Spawner>,
    id: MailboxId,
    subname: String,
    sender: mpsc::Sender<Envelope>,
    binding: Arc<NativeBinding>,
    slots: Box<ActorSlots>,
    state: A::State,
}

impl<A: NativeActor> LegacyPreparedActivation<A> {
    pub(super) fn new(
        spawner: Arc<Spawner>,
        id: MailboxId,
        subname: String,
        sender: mpsc::Sender<Envelope>,
        binding: Arc<NativeBinding>,
        slots: Box<ActorSlots>,
        state: A::State,
    ) -> Self {
        Self { spawner, id, subname, sender, binding, slots, state }
    }
}

impl<A: NativeActor> PreparedSpawnActivation for LegacyPreparedActivation<A> {
    fn reserve(
        self: Box<Self>,
        token: ActivationToken,
    ) -> Result<Arc<dyn ActivationReservation>, Box<dyn PreparedSpawnActivation>> {
        if !self.spawner.actor_registry().reserve_starting(self.id, token) {
            return Err(self);
        }
        Ok(Arc::new(LegacyActivationControl {
            actor_registry: Arc::clone(self.spawner.actor_registry()),
            registry: Arc::clone(self.spawner.registry()),
            id: self.id,
            token,
            prepared: Mutex::new(Some(*self)),
            live: Arc::new(Mutex::new(None)),
            done: Mutex::new(None),
            cancel_done: Arc::new(Mutex::new(None)),
            barrier_mail_id: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
        }))
    }

    fn discard_at_home(self: Box<Self>) -> crossbeam_channel::Receiver<()> {
        let sink = self.spawner.wake_sink().clone();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let job: Arc<dyn Drainable> = Arc::new(DiscardJob {
            prepared: Mutex::new(Some(*self)),
            done: Mutex::new(Some(done_tx)),
            ran: AtomicBool::new(false),
        });
        sink.schedule(job);
        done_rx
    }
}

struct DiscardJob<A: NativeActor> {
    prepared: Mutex<Option<LegacyPreparedActivation<A>>>,
    done: Mutex<Option<crossbeam_channel::Sender<()>>>,
    ran: AtomicBool,
}

impl<A: NativeActor> Drainable for DiscardJob<A> {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if !self.ran.swap(true, Ordering::AcqRel) {
            drop(self.prepared.lock().expect("prepared activation discard lock poisoned").take());
            let done = self.done.lock().expect("prepared activation discard completion lock poisoned").take();
            if let Some(done) = done {
                let _ = done.send(());
            }
        }
        CycleResult::Closed
    }

    fn label(&self) -> &'static str {
        "native-activation-discard"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct LegacyActivationControl<A: NativeActor> {
    actor_registry: Arc<ActorRegistry>,
    registry: Arc<Registry>,
    id: MailboxId,
    token: ActivationToken,
    prepared: Mutex<Option<LegacyPreparedActivation<A>>>,
    live: Arc<Mutex<Option<Box<dyn LiveActivation>>>>,
    done: Mutex<Option<crossbeam_channel::Receiver<()>>>,
    cancel_done: Arc<Mutex<Option<crossbeam_channel::Receiver<()>>>>,
    barrier_mail_id: Arc<Mutex<Option<MailId>>>,
    cancelled: Arc<AtomicBool>,
}

impl<A: NativeActor> ActivationReservation for LegacyActivationControl<A> {
    fn schedule(&self) {
        let prepared = self.prepared.lock().expect("activation preparation lock poisoned").take();
        let Some(prepared) = prepared else {
            return;
        };
        let sink = prepared.spawner.wake_sink().clone();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        self.done.lock().expect("activation completion lock poisoned").replace(done_rx);
        let job = Arc::new(ActivationJob {
            prepared: Mutex::new(Some(prepared)),
            live: Arc::clone(&self.live),
            cancelled: Arc::clone(&self.cancelled),
            cancel_done: Arc::clone(&self.cancel_done),
            barrier_mail_id: Arc::clone(&self.barrier_mail_id),
            token: self.token,
            id: self.id,
            registry: Arc::clone(&self.registry),
            actor_registry: Arc::clone(&self.actor_registry),
            done: Mutex::new(Some(done_tx)),
            ran: AtomicBool::new(false),
        });
        let erased: Arc<dyn Drainable> = job;
        sink.schedule(erased);
    }

    fn take_live(&self) -> Option<Box<dyn LiveActivation>> {
        self.live.lock().expect("activation live lock poisoned").take()
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.schedule();
        if let Some(live) = self.take_live() {
            self.cancel_done.lock().expect("activation cancel completion lock poisoned").replace(live.cancel_at_home());
        }
    }

    fn join(&self) {
        let done = self.done.lock().expect("activation completion lock poisoned").take();
        if let Some(done) = done {
            let _ = done.recv();
        }
        if let Some(live) = self.take_live() {
            self.cancel_done.lock().expect("activation cancel completion lock poisoned").replace(live.cancel_at_home());
        }
        let done = self.cancel_done.lock().expect("activation cancel completion lock poisoned").take();
        if let Some(done) = done {
            let _ = done.recv();
        }
        self.actor_registry.rollback_starting(self.id, self.token);
    }

    fn barrier_matches(&self, mail_id: MailId) -> bool {
        *self.barrier_mail_id.lock().expect("activation barrier identity lock poisoned") == Some(mail_id)
    }
}

struct ActivationJob<A: NativeActor> {
    prepared: Mutex<Option<LegacyPreparedActivation<A>>>,
    live: Arc<Mutex<Option<Box<dyn LiveActivation>>>>,
    cancelled: Arc<AtomicBool>,
    cancel_done: Arc<Mutex<Option<crossbeam_channel::Receiver<()>>>>,
    barrier_mail_id: Arc<Mutex<Option<MailId>>>,
    token: ActivationToken,
    id: MailboxId,
    registry: Arc<Registry>,
    actor_registry: Arc<ActorRegistry>,
    done: Mutex<Option<crossbeam_channel::Sender<()>>>,
    ran: AtomicBool,
}

impl<A: NativeActor> ActivationJob<A> {
    fn finish(&self) {
        let done = self.done.lock().expect("activation completion lock poisoned").take();
        if let Some(done) = done {
            let _ = done.send(());
        }
    }
}

impl<A: NativeActor> Drainable for ActivationJob<A> {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if self.ran.swap(true, Ordering::AcqRel) {
            return CycleResult::Closed;
        }
        let Some(prepared) = self.prepared.lock().expect("activation preparation lock poisoned").take() else {
            self.finish();
            return CycleResult::Closed;
        };
        if self.cancelled.load(Ordering::Acquire) {
            drop(prepared);
            self.actor_registry.rollback_starting(self.id, self.token);
            self.registry.activation_cancelled(self.id, self.token);
            self.finish();
            return CycleResult::Closed;
        }

        let live = LegacyLiveActivation::wire(prepared, self.token);
        if self.cancelled.load(Ordering::Acquire) {
            live.cancel_here();
            self.actor_registry.rollback_starting(self.id, self.token);
            self.registry.activation_cancelled(self.id, self.token);
        } else {
            let binding = Arc::clone(&live.binding);
            let id = live.id;
            self.live.lock().expect("activation live lock poisoned").replace(Box::new(live));
            // The live lease is visible to the owner before the barrier can
            // leave this execution home.
            let barrier_mail_id = binding.push_envelope_buffered(
                id.0,
                ACTIVATION_BARRIER_KIND.0,
                &self.token.value().to_le_bytes(),
                1,
                None,
                None,
            );
            self.barrier_mail_id.lock().expect("activation barrier identity lock poisoned").replace(barrier_mail_id);
            binding.flush_outbound();
            if self.cancelled.load(Ordering::Acquire)
                && let Some(live) = self.live.lock().expect("activation live lock poisoned").take()
            {
                self.cancel_done
                    .lock()
                    .expect("activation cancel completion lock poisoned")
                    .replace(live.cancel_at_home());
            }
        }
        self.finish();
        CycleResult::Closed
    }

    fn label(&self) -> &'static str {
        "native-activation"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct LegacyLiveActivation<A: NativeActor> {
    spawner: Arc<Spawner>,
    id: MailboxId,
    token: ActivationToken,
    subname: String,
    sender: mpsc::Sender<Envelope>,
    strong_sender: Arc<mpsc::Sender<Envelope>>,
    binding: Arc<NativeBinding>,
    slot: Arc<DispatcherSlot<A>>,
}

impl<A: NativeActor> LegacyLiveActivation<A> {
    fn wire(prepared: LegacyPreparedActivation<A>, token: ActivationToken) -> Self {
        let LegacyPreparedActivation { spawner, id, subname, sender, binding, slots, state } = prepared;
        let slot = DispatcherSlot::new(
            Box::new(state),
            Arc::clone(&binding),
            slots,
            Arc::clone(spawner.actor_registry()),
            Arc::clone(spawner.mailer()),
            id,
        );
        slot.wire_activation();

        Self { spawner, id, token, subname, sender: sender.clone(), strong_sender: Arc::new(sender), binding, slot }
    }

    fn cancel_here(self) {
        self.slot.cancel_activation();
    }
}

impl<A: NativeActor> LiveActivation for LegacyLiveActivation<A> {
    fn install(self: Box<Self>, bootstrap: Vec<PreparedMail>, parked: Vec<PreparedMail>) -> InstalledActivation {
        let Self { spawner, id, token, subname, sender, strong_sender, binding: _, slot } = *self;
        for prepared in bootstrap.into_iter().chain(parked) {
            let PreparedMail { mail, kind_name, bootstrap } = prepared;
            let t_enqueue = if bootstrap {
                Nanos(0)
            } else {
                spawner.mailer().now_nanos()
            };
            let depth = if bootstrap {
                0
            } else {
                pending_depth()
            };
            let envelope = if bootstrap {
                OwnedDispatch::disarmed(
                    mail.kind,
                    kind_name,
                    None,
                    mail.reply_to,
                    mail.payload,
                    mail.count,
                    mail.mail_id,
                    mail.root,
                    mail.parent_mail,
                    t_enqueue,
                    depth,
                    id,
                )
            } else {
                OwnedDispatch::armed(
                    mail.kind,
                    kind_name,
                    None,
                    mail.reply_to,
                    mail.payload,
                    mail.count,
                    mail.mail_id,
                    mail.root,
                    mail.parent_mail,
                    t_enqueue,
                    depth,
                    id,
                )
            };
            let _ = sender.send(envelope);
        }

        let wake_slot = Arc::new(MailboxWakeSlot::default());
        let seize_cell = SeizeCell::default();
        let weak_sender = Arc::downgrade(&strong_sender);
        let handler_wake = Arc::clone(&wake_slot);
        let entry = MailboxEntry::Inbox {
            handler: Arc::new(move |dispatch: OwnedDispatch| {
                match relay_or_transfer(dispatch, &weak_sender, &handler_wake) {
                    RelayOutcome::Delivered => {}
                    RelayOutcome::SenderGone { kind_name } | RelayOutcome::ReceiverGone { kind_name } => {
                        tracing::warn!(target: "aether_substrate::spawn", kind = %kind_name, "activating actor discarded mail");
                    }
                }
            }),
            seize: Arc::clone(&seize_cell),
        };

        spawner.actor_registry().promote_starting(id, token, strong_sender, TypeId::of::<A>(), subname);
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let seize = SeizeHandle::new(Arc::clone(slot.state()), Arc::downgrade(&slot_dyn));
        let wake = WakeHandle::new(Arc::clone(slot.state()), Arc::downgrade(&slot_dyn), spawner.wake_sink().clone());
        spawner.retain_activated_slot(id, slot_dyn, wake.clone());
        let catch_up = Box::new(move || {
            wake_slot.set(Arc::new({
                let wake = wake.clone();
                move || {
                    let _ = wake.wake();
                }
            }));
            let _ = wake.wake();
            assert!(seize_cell.set(seize).is_ok(), "fresh activation seize cell accepts its handle");
        });

        InstalledActivation { entry, catch_up }
    }

    fn cancel_at_home(self: Box<Self>) -> crossbeam_channel::Receiver<()> {
        let sink = self.spawner.wake_sink().clone();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let registry = Arc::clone(self.spawner.registry());
        let actor_registry = Arc::clone(self.spawner.actor_registry());
        let id = self.id;
        let token = self.token;
        let job: Arc<dyn Drainable> = Arc::new(CancelJob {
            live: Mutex::new(Some(*self)),
            registry,
            actor_registry,
            id,
            token,
            done: Mutex::new(Some(done_tx)),
            ran: AtomicBool::new(false),
        });
        sink.schedule(job);
        done_rx
    }
}

struct CancelJob<A: NativeActor> {
    live: Mutex<Option<LegacyLiveActivation<A>>>,
    done: Mutex<Option<crossbeam_channel::Sender<()>>>,
    registry: Arc<Registry>,
    actor_registry: Arc<ActorRegistry>,
    id: MailboxId,
    token: ActivationToken,
    ran: AtomicBool,
}

impl<A: NativeActor> Drainable for CancelJob<A> {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if !self.ran.swap(true, Ordering::AcqRel) {
            let live = self.live.lock().expect("activation cancel lock poisoned").take();
            if let Some(live) = live {
                live.cancel_here();
            }
            self.actor_registry.rollback_starting(self.id, self.token);
            self.registry.activation_cancelled(self.id, self.token);
            let done = self.done.lock().expect("activation cancel completion lock poisoned").take();
            if let Some(done) = done {
                let _ = done.send(());
            }
        }
        CycleResult::Closed
    }

    fn label(&self) -> &'static str {
        "native-activation-cancel"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
