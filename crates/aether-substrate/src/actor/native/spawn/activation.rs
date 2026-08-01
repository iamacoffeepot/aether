//! Private legacy adapter for ADR-0165 staged actor activation.

use std::any::{Any, TypeId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};

use aether_actor::local::ActorSlots;

use super::reservation::ParentReservation;
use super::{SpawnError, SpawnOutcome};
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::offload::blocking::DeferredCompletion;
use crate::actor::native::slot::dispatcher::DispatcherSlot;
use crate::actor::native::{Envelope, NativeActor};
use crate::actor::registry::ActorRegistry;
use crate::chassis::ctx::{MailboxWakeSlot, RelayOutcome, relay_or_transfer};
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{
    ACTIVATION_BARRIER_KIND, ActivationReservation, ActivationToken, InstalledActivation, LiveActivation, PreparedMail,
    PreparedSpawnActivation, PreparedSpawnFailure,
};
use crate::mail::registry::{MailboxEntry, OwnedDispatch, Registry, SeizeCell};
use crate::mail::{Mail, MailId, MailboxId};
use crate::runtime::effect_chain::EffectChain;
use crate::scheduler::pending_depth;
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SeizeHandle, WakeHandle};
use aether_kinds::trace::Nanos;

use super::Spawner;

pub(super) struct LegacyPreparedActivation<A: NativeActor> {
    spawner: Arc<Spawner>,
    id: MailboxId,
    subname: String,
    sender: mpsc::Sender<Envelope>,
    binding: Arc<NativeBinding>,
    slots: Box<ActorSlots>,
    state: A::State,
    finalizer: Option<Arc<NativeSpawnFinalizer>>,
    /// The staging site's ADR-0168 §3 declaration, carried to the activation
    /// home so `wire` can attach a birth-completing effect to whatever chain
    /// it names.
    chain: EffectChain,
}

pub(super) struct NativeSpawnFinalizer {
    state: Mutex<Option<NativeSpawnFinalizerState>>,
    retained: Mutex<Vec<(MailId, MailId)>>,
    mailer: Arc<Mailer>,
}

/// Where one birth's authoritative fate is delivered.
///
/// A handler-staged child completes into its parent actor's mailbox as the
/// ADR-0093 `TaskDone<SpawnOutcome, _>`. A post-seal external birth has no
/// parent actor — its caller is an embedder thread reaching in through
/// `PassiveChassis::spawn_actor` — so it has no mailbox to receive that, and
/// completes into a channel that thread is blocked on instead. The same
/// two-audience split `RegistryBatchCompletionSink` draws for owner batches
/// one layer up: actors get mail, external threads get a channel.
pub(super) enum SpawnCompletionSink {
    Deferred(DeferredCompletion<SpawnOutcome>),
    Channel(crossbeam_channel::Sender<SpawnOutcome>),
}

impl SpawnCompletionSink {
    fn complete(self, outcome: SpawnOutcome) {
        match self {
            Self::Deferred(completion) => completion.complete(outcome),
            Self::Channel(sender) => drop(sender.send(outcome)),
        }
    }
}

/// The parent-local staged key one birth holds, paired with the child binding
/// that takes ownership of it once the birth is Live. Absent for a post-seal
/// external birth, which has no parent actor to hold a key for it.
struct ParentLink {
    reservation: ParentReservation,
    child: Weak<NativeBinding>,
}

struct NativeSpawnFinalizerState {
    parent: Option<ParentLink>,
    completion: SpawnCompletionSink,
    /// The staged child's identity, carried onto **both** arms of the
    /// [`SpawnOutcome`] so a rejection names the birth it belongs to.
    mailbox_id: MailboxId,
    canonical_name: Arc<str>,
}

impl NativeSpawnFinalizer {
    /// A handler-staged child birth: the parent holds a local reservation key
    /// for it and receives the outcome as an ADR-0093 `TaskDone`.
    pub(super) fn parented(
        parent_reservation: ParentReservation,
        completion: DeferredCompletion<SpawnOutcome>,
        mailbox_id: MailboxId,
        canonical_name: Arc<str>,
        child: Weak<NativeBinding>,
        mailer: Arc<Mailer>,
    ) -> Arc<Self> {
        Self::new(
            Some(ParentLink { reservation: parent_reservation, child }),
            SpawnCompletionSink::Deferred(completion),
            mailbox_id,
            canonical_name,
            mailer,
        )
    }

    /// A post-seal external birth (ADR-0165): no parent actor holds a key for
    /// it, and the embedder thread that submitted it is blocked on `outcome`
    /// until this finalizer decides.
    pub(super) fn external(
        outcome: crossbeam_channel::Sender<SpawnOutcome>,
        mailbox_id: MailboxId,
        canonical_name: Arc<str>,
        mailer: Arc<Mailer>,
    ) -> Arc<Self> {
        Self::new(None, SpawnCompletionSink::Channel(outcome), mailbox_id, canonical_name, mailer)
    }

    fn new(
        parent: Option<ParentLink>,
        completion: SpawnCompletionSink,
        mailbox_id: MailboxId,
        canonical_name: Arc<str>,
        mailer: Arc<Mailer>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Some(NativeSpawnFinalizerState { parent, completion, mailbox_id, canonical_name })),
            retained: Mutex::new(Vec::new()),
            mailer,
        })
    }

    fn retain(&self, mail: &Mail) {
        if mail.mail_id != MailId::NONE {
            self.retained.lock().expect("native spawn retained-mail lock poisoned").push((mail.mail_id, mail.root));
        }
    }

    fn reject(&self, failure: PreparedSpawnFailure) {
        let Some(state) = self.state.lock().expect("native spawn finalizer lock poisoned").take() else {
            return;
        };
        for (mail_id, root) in self.retained.lock().expect("native spawn retained-mail lock poisoned").drain(..) {
            self.mailer.record_finished(mail_id, root);
        }
        if let Some(parent) = state.parent {
            parent.reservation.reject();
        }
        let error = match failure {
            PreparedSpawnFailure::NamespaceOwnedByOtherType { namespace, owning_type } => {
                SpawnError::NamespaceOwnedByOtherType { namespace, owning_type }
            }
            PreparedSpawnFailure::SubnameRetired { full_name } => SpawnError::SubnameRetired { full_name },
            PreparedSpawnFailure::SubnameInUse { full_name } => SpawnError::SubnameInUse { full_name },
            PreparedSpawnFailure::ActivationRejected => SpawnError::ActivationRejected,
            PreparedSpawnFailure::OwnerClosed => SpawnError::OwnerClosed,
        };
        state.completion.complete(SpawnOutcome {
            mailbox_id: state.mailbox_id,
            canonical_name: state.canonical_name,
            result: Err(error),
        });
    }

    fn promote(&self) {
        let Some(state) = self.state.lock().expect("native spawn finalizer lock poisoned").take() else {
            return;
        };
        if let Some(parent) = state.parent {
            let live = parent.reservation.promote();
            if let Some(child) = parent.child.upgrade() {
                child.retain_parent_child_reservation(live);
            }
        }
        state.completion.complete(SpawnOutcome {
            mailbox_id: state.mailbox_id,
            canonical_name: state.canonical_name,
            result: Ok(()),
        });
    }
}

impl<A: NativeActor> LegacyPreparedActivation<A> {
    /// `chain` is the staging site's ADR-0168 §3 declaration, carried to the
    /// activation home so `wire` can attach a birth-completing effect to
    /// whatever chain it names. Deliberately an argument rather than a
    /// builder step with a default: a default would be one of the three
    /// answers, and picking one silently is the shape the requirement exists
    /// to remove.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a distinct piece of the prepared birth; the ADR-0168 declaration is required, not defaulted"
    )]
    pub(super) fn new(
        spawner: Arc<Spawner>,
        id: MailboxId,
        subname: String,
        sender: mpsc::Sender<Envelope>,
        binding: Arc<NativeBinding>,
        slots: Box<ActorSlots>,
        state: A::State,
        chain: EffectChain,
    ) -> Self {
        Self { spawner, id, subname, sender, binding, slots, state, finalizer: None, chain }
    }

    pub(super) fn with_finalizer(mut self, finalizer: Arc<NativeSpawnFinalizer>) -> Self {
        self.finalizer = Some(finalizer);
        self
    }
}

impl<A: NativeActor> PreparedSpawnActivation for LegacyPreparedActivation<A> {
    fn reserve(
        self: Box<Self>,
        token: ActivationToken,
    ) -> Result<Arc<dyn ActivationReservation>, (Box<dyn PreparedSpawnActivation>, PreparedSpawnFailure)> {
        if let Err(owning_type) = self.spawner.actor_registry().try_claim_namespace(A::NAMESPACE, TypeId::of::<A>()) {
            return Err((
                self,
                PreparedSpawnFailure::NamespaceOwnedByOtherType { namespace: A::NAMESPACE, owning_type },
            ));
        }
        if self.spawner.actor_registry().is_tombstoned(self.id) {
            let full_name =
                self.binding.runtime_identity().expect("prepared binding is typed").canonical_name().to_string();
            return Err((self, PreparedSpawnFailure::SubnameRetired { full_name }));
        }
        if !self.spawner.actor_registry().reserve_starting(self.id, token) {
            let full_name =
                self.binding.runtime_identity().expect("prepared binding is typed").canonical_name().to_string();
            return Err((self, PreparedSpawnFailure::SubnameInUse { full_name }));
        }
        let failure = Arc::new(Mutex::new(None));
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
            failure,
        }))
    }

    fn discard_at_home(self: Box<Self>, failure: PreparedSpawnFailure) -> crossbeam_channel::Receiver<()> {
        let sink = self.spawner.wake_sink().clone();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let job: Arc<dyn Drainable> = Arc::new(DiscardJob {
            prepared: Mutex::new(Some(*self)),
            done: Mutex::new(Some(done_tx)),
            ran: AtomicBool::new(false),
            failure: Mutex::new(Some(failure)),
        });
        sink.schedule(job);
        done_rx
    }

    fn retain_mail(&mut self, mail: &Mail) {
        if let Some(finalizer) = &self.finalizer {
            finalizer.retain(mail);
        }
    }

    fn id_is_retired(&self) -> bool {
        self.spawner.actor_registry().is_tombstoned(self.id)
    }
}

struct DiscardJob<A: NativeActor> {
    prepared: Mutex<Option<LegacyPreparedActivation<A>>>,
    done: Mutex<Option<crossbeam_channel::Sender<()>>>,
    ran: AtomicBool,
    failure: Mutex<Option<PreparedSpawnFailure>>,
}

impl<A: NativeActor> Drainable for DiscardJob<A> {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        if !self.ran.swap(true, Ordering::AcqRel) {
            let prepared = self.prepared.lock().expect("prepared activation discard lock poisoned").take();
            if let Some(prepared) = prepared {
                let finalizer = prepared.finalizer.as_ref().map(Arc::clone);
                drop(prepared);
                if let Some(finalizer) = finalizer {
                    finalizer.reject(
                        self.failure
                            .lock()
                            .expect("prepared activation failure lock poisoned")
                            .take()
                            .unwrap_or(PreparedSpawnFailure::ActivationRejected),
                    );
                }
            }
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
    failure: Arc<Mutex<Option<PreparedSpawnFailure>>>,
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
            failure: Arc::clone(&self.failure),
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

    fn reject(&self, failure: PreparedSpawnFailure) {
        let mut slot = self.failure.lock().expect("activation failure lock poisoned");
        if slot.is_none() {
            *slot = Some(failure);
        }
        drop(slot);
        self.cancel();
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
    failure: Arc<Mutex<Option<PreparedSpawnFailure>>>,
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
            let finalizer = prepared.finalizer.as_ref().map(Arc::clone);
            drop(prepared);
            self.actor_registry.rollback_starting(self.id, self.token);
            self.registry.activation_cancelled(self.id, self.token);
            if let Some(finalizer) = finalizer {
                finalizer.reject(
                    self.failure
                        .lock()
                        .expect("activation failure lock poisoned")
                        .take()
                        .unwrap_or(PreparedSpawnFailure::ActivationRejected),
                );
            }
            self.finish();
            return CycleResult::Closed;
        }

        let live = LegacyLiveActivation::wire(prepared, self.token, Arc::clone(&self.failure));
        if self.cancelled.load(Ordering::Acquire) {
            live.cancel_here();
            self.actor_registry.rollback_starting(self.id, self.token);
            self.registry.activation_cancelled(self.id, self.token);
        } else {
            let binding = Arc::clone(&live.binding);
            let id = live.id;
            self.live.lock().expect("activation live lock poisoned").replace(Box::new(live));
            // The live lease is visible to the owner before the barrier can
            // leave this execution home. This substrate-owned control mail is
            // intentionally eager; buffered wire work stays behind the
            // binding's activation hold until the owner's post-Live suffix.
            binding.push_envelope_returning_root_before_push(
                id.0,
                ACTIVATION_BARRIER_KIND.0,
                &self.token.value().to_le_bytes(),
                1,
                None,
                None,
                |barrier_mail_id| {
                    self.barrier_mail_id
                        .lock()
                        .expect("activation barrier identity lock poisoned")
                        .replace(barrier_mail_id);
                },
            );
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
    finalizer: Option<Arc<NativeSpawnFinalizer>>,
    failure: Arc<Mutex<Option<PreparedSpawnFailure>>>,
}

impl<A: NativeActor> LegacyLiveActivation<A> {
    fn wire(
        prepared: LegacyPreparedActivation<A>,
        token: ActivationToken,
        failure: Arc<Mutex<Option<PreparedSpawnFailure>>>,
    ) -> Self {
        let LegacyPreparedActivation { spawner, id, subname, sender, binding, slots, state, finalizer, chain } =
            prepared;
        let slot = DispatcherSlot::new(
            Box::new(state),
            Arc::clone(&binding),
            slots,
            Arc::clone(spawner.actor_registry()),
            Arc::clone(spawner.mailer()),
            id,
        );
        binding.hold_outbound_for_activation();
        slot.wire_activation(chain);

        Self {
            spawner,
            id,
            token,
            subname,
            sender: sender.clone(),
            strong_sender: Arc::new(sender),
            binding,
            slot,
            finalizer,
            failure,
        }
    }

    fn cancel_here(self) {
        let finalizer = self.finalizer.as_ref().map(Arc::clone);
        self.slot.cancel_activation();
        self.binding.discard_outbound_after_activation();
        if let Some(finalizer) = finalizer {
            finalizer.reject(
                self.failure
                    .lock()
                    .expect("activation failure lock poisoned")
                    .take()
                    .unwrap_or(PreparedSpawnFailure::ActivationRejected),
            );
        }
    }
}

impl<A: NativeActor> LiveActivation for LegacyLiveActivation<A> {
    fn install(self: Box<Self>, bootstrap: Vec<PreparedMail>, parked: Vec<PreparedMail>) -> InstalledActivation {
        let Self { spawner, id, token, subname, sender, strong_sender, binding, slot, finalizer, failure: _ } = *self;
        for prepared in bootstrap.into_iter().chain(parked) {
            let PreparedMail { mail, bootstrap } = prepared;
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
                    RelayOutcome::SenderGone { kind } | RelayOutcome::ReceiverGone { kind } => {
                        tracing::warn!(target: "aether_substrate::spawn", kind = %kind, "activating actor discarded mail");
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
            // The owner has published the Live route and released the
            // registry publication lock before invoking this suffix. Owner
            // serialization may remain held, so this flush must only submit
            // or schedule work; it must never wait for owner application.
            // Keep the actor unwakeable until that work is queued, then expose
            // its wake/seize machinery. Self-mail and replies therefore queue
            // behind the parked prefix without creating a second ring
            // producer.
            binding.release_outbound_after_activation();
            wake_slot.set(Arc::new({
                let wake = wake.clone();
                move || {
                    let _ = wake.wake();
                }
            }));
            let _ = wake.wake();
            assert!(seize_cell.set(seize).is_ok(), "fresh activation seize cell accepts its handle");
            if let Some(finalizer) = finalizer {
                finalizer.promote();
            }
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
