//! Fixtures shared across the spawn test siblings: the probe actor whose
//! lifecycle events the activation assertions read, and the prepared-birth
//! constructors that stand a birth up at each stage of the path.

use std::sync::Arc;
use std::thread;

use aether_actor::{ActorRef, Addressable, HandlesKind, Many};
use aether_data::{ActorId, Kind as _, RequestId};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::spawn::activation::NativeSpawnFinalizer;
use crate::actor::native::spawn::reservation::ChildReservationKey;
use crate::actor::native::spawn::{SpawnOutcome, Spawner, Subname};
use crate::actor::native::{DispatchId, NativeActor, NativeCtx, NativeInitCtx, TaskCompletionWake, TaskDone};
use crate::chassis::error::BootError;
use crate::config::RingCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::PreparedSpawnCommit;
use crate::mail::registry::{MailDispatch, OwnedDispatch, Registry};
use crate::mail::{KindId, MailId};
use crate::runtime::effect_chain::{EffectChain, Uncaused};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{Pool, PoolConfig, PoolHandle};
use crate::testing::{await_event, boot_authority};

#[aether_data::kind(name = "test.activation.poke", copy)]
pub(super) struct ActivationPoke;

/// Drives the probe down its ordinary self-close path — the handler flips
/// the shutdown flag its dispatcher slot polls, exactly as a production
/// actor that retires itself does.
#[aether_data::kind(name = "test.activation.close", copy)]
pub(super) struct ActivationClose;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActivationEvent {
    Wire(thread::ThreadId),
    Dispatch(thread::ThreadId),
    Unwire(thread::ThreadId),
    Drop(thread::ThreadId),
}

/// What an [`activation_sink`] stands in for: the peer a probe pokes from its
/// `wire` and `unwire`. The sink's inline route takes any kind, so its proof is
/// typed as this marker, which handles only the poke.
pub(super) struct PokeSink;

impl Addressable for PokeSink {
    const NAMESPACE: &'static str = "test.activation.poke_sink";
    type Resolver = Many;
}

impl HandlesKind<ActivationPoke> for PokeSink { type Sender = aether_actor::Anyone; }

pub(super) struct ActivationProbe {
    events: crossbeam_channel::Sender<ActivationEvent>,
    lifecycle_target: Option<ActorRef<PokeSink>>,
}

pub(super) struct ActivationConfig {
    events: crossbeam_channel::Sender<ActivationEvent>,
    lifecycle_target: Option<ActorRef<PokeSink>>,
}

impl ActivationConfig {
    pub(super) fn new(events: crossbeam_channel::Sender<ActivationEvent>) -> Self {
        Self { events, lifecycle_target: None }
    }

    pub(super) fn with_lifecycle_target(
        events: crossbeam_channel::Sender<ActivationEvent>,
        lifecycle_target: ActorRef<PokeSink>,
    ) -> Self {
        Self { events, lifecycle_target: Some(lifecycle_target) }
    }
}

impl Drop for ActivationProbe {
    fn drop(&mut self) {
        let _ = self.events.send(ActivationEvent::Drop(thread::current().id()));
    }
}

#[aether_actor::actor(instanced, root)]
impl NativeActor for ActivationProbe {
    const NAMESPACE: &'static str = "test.activation.probe";
    type Config = ActivationConfig;

    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { events: config.events, lifecycle_target: config.lifecycle_target })
    }

    fn wire(state: &mut Self, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        if let Some(target) = state.lifecycle_target {
            ctx.send_detached_to(target, &ActivationPoke);
        }
        let _ = state.events.send(ActivationEvent::Wire(thread::current().id()));
        Ok(())
    }

    #[handler::tell]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _poke: ActivationPoke) {
        let _ = self.events.send(ActivationEvent::Dispatch(thread::current().id()));
    }

    #[handler::tell]
    fn on_close(&mut self, ctx: &mut NativeCtx<'_>, _close: ActivationClose) {
        let _ = self.events.send(ActivationEvent::Dispatch(thread::current().id()));
        ctx.shutdown();
    }

    fn unwire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
        if let Some(target) = state.lifecycle_target {
            ctx.send_detached_to(target, &ActivationPoke);
        }
        let _ = state.events.send(ActivationEvent::Unwire(thread::current().id()));
    }
}

pub(super) fn activation_fixture() -> (Arc<Spawner>, Arc<Registry>, Arc<Mailer>, PoolHandle) {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, Arc::clone(&aborter));
    let spawner = Arc::new(Spawner::new(
        Arc::clone(&registry),
        Arc::clone(&mailer),
        aborter,
        pool.wake_sink(),
        RingCapacities::default(),
    ));
    (spawner, registry, mailer, pool)
}

pub(super) fn prepared_probe(
    spawner: &Arc<Spawner>,
    name: &str,
    events: crossbeam_channel::Sender<ActivationEvent>,
) -> PreparedSpawnCommit {
    let identity = spawner.preflight::<ActivationProbe>(Subname::Named(name), None).unwrap();
    let staged = spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events), (), Vec::new()).unwrap();
    spawner.prepare_commit(staged, None, EffectChain::Uncaused(Uncaused::EmbedderCall))
}

pub(super) fn prepared_probe_with_lifecycle_target(
    spawner: &Arc<Spawner>,
    name: &str,
    events: crossbeam_channel::Sender<ActivationEvent>,
    lifecycle_target: ActorRef<PokeSink>,
) -> PreparedSpawnCommit {
    let identity = spawner.preflight::<ActivationProbe>(Subname::Named(name), None).unwrap();
    let staged = spawner
        .build::<ActivationProbe>(
            identity,
            ActivationConfig::with_lifecycle_target(events, lifecycle_target),
            (),
            Vec::new(),
        )
        .unwrap();
    spawner.prepare_commit(staged, None, EffectChain::Uncaused(Uncaused::EmbedderCall))
}

pub(super) fn activation_sink(
    registry: &Registry,
    name: &str,
) -> (ActorRef<PokeSink>, crossbeam_channel::Receiver<KindId>) {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let sink = registry.register_inline(
        &boot_authority(),
        name,
        Arc::new(move |dispatch: MailDispatch<'_>| {
            let _ = sender.send(dispatch.kind);
        }),
    );
    (Registry::declared_dependency::<PokeSink>(sink.id()), receiver)
}

pub(super) fn finalized_probe(
    spawner: &Arc<Spawner>,
    parent: &Arc<NativeBinding>,
    name: &str,
    events: crossbeam_channel::Sender<ActivationEvent>,
    correlation: u64,
) -> (PreparedSpawnCommit, DispatchId, ChildReservationKey) {
    let key = ChildReservationKey::new(
        parent.self_mailbox(),
        ActorId::singleton(ActivationProbe::NAMESPACE),
        ActorId::instanced(ActivationProbe::NAMESPACE, name),
    );
    let parent_reservation = parent.reserve_child(key).expect("distinct staged parent key reservation wins");
    let identity = spawner.prepare_identity::<ActivationProbe>(Subname::Named(name), None).unwrap();
    let staged = spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events), (), Vec::new()).unwrap();
    let causing_chain = MailId::new(parent.self_mailbox(), correlation);
    let deferred = parent.dispatch_stage::<SpawnOutcome<ActivationProbe>>(
        Some(spawner.mailer().acquire_settlement_hold(causing_chain)),
        RequestId(parent.mint_correlation()),
    );
    let dispatch_id = deferred.dispatch_id();
    let finalizer = NativeSpawnFinalizer::<ActivationProbe>::parented(
        parent_reservation,
        deferred,
        staged.identity.id,
        staged.identity.canonical_name.clone(),
        Arc::downgrade(&staged.transport),
    );

    (spawner.prepare_commit(staged, Some(finalizer), EffectChain::Held(causing_chain)), dispatch_id, key)
}

/// A parent binding whose own mailbox is registered as `name`, beside the
/// channel its completion wakes arrive on.
///
/// A finalized birth tells its parent the outcome the way any staged task
/// does: it fills the parent's ledger and then pushes one
/// [`TaskCompletionWake`] to the parent's mailbox. The route registered here
/// forwards each wake's [`DispatchId`], so [`await_spawn_done`] waits on the
/// wake a parent actor would be woken by.
pub(super) fn activation_parent(
    registry: &Registry,
    mailer: &Arc<Mailer>,
    name: &str,
) -> (Arc<NativeBinding>, crossbeam_channel::Receiver<DispatchId>) {
    let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
    let mailbox = registry.register_inbox(
        &boot_authority(),
        name,
        Arc::new(move |dispatch: OwnedDispatch| {
            // ADR-0094: terminal test consumer.
            dispatch.discharge();
            let wake = TaskCompletionWake::decode_from_bytes(dispatch.payload.bytes())
                .expect("only a completion wake reaches the activation parent");
            let _ = wake_tx.send(DispatchId(wake.dispatch_id));
        }),
    );

    (Arc::new(NativeBinding::new_for_test(Arc::clone(mailer), mailbox)), wake_rx)
}

/// Take the outcome `dispatch_id` names, waiting for a completion wake when
/// the ledger does not hold it yet.
///
/// The finalizer fills the ledger before it pushes the wake, so an outcome
/// that is missing has not sent its wake: the wait always has one coming,
/// and a wake for another birth only sends the loop round to look again.
pub(super) fn await_spawn_done(
    parent: &NativeBinding,
    wakes: &crossbeam_channel::Receiver<DispatchId>,
    dispatch_id: DispatchId,
) -> TaskDone<SpawnOutcome<ActivationProbe>, ()> {
    loop {
        if let Some(done) = parent.dispatch_take(dispatch_id) {
            return done;
        }
        await_event(wakes, "test.activation.spawn_done");
    }
}
