//! Fixtures shared across the spawn test siblings: the probe actor whose
//! lifecycle events the activation assertions read, and the prepared-birth
//! constructors that stand a birth up at each stage of the path.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_data::{ActorId, Kind};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::spawn::activation::NativeSpawnFinalizer;
use crate::actor::native::spawn::reservation::ChildReservationKey;
use crate::actor::native::spawn::{SpawnOutcome, Spawner, Subname};
use crate::actor::native::{DispatchId, NativeActor, NativeCtx, NativeInitCtx, TaskDone};
use crate::actor::registry::ActorRegistry;
use crate::chassis::error::BootError;
use crate::config::RingCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::PreparedSpawnCommit;
use crate::mail::registry::{MailDispatch, Registry};
use crate::mail::{KindId, MailId, MailboxId, Source};
use crate::runtime::effect_chain::{EffectChain, Uncaused};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{Pool, PoolConfig, PoolHandle};
use crate::testing::boot_authority;

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

pub(super) struct ActivationProbe {
    events: crossbeam_channel::Sender<ActivationEvent>,
    lifecycle_target: Option<MailboxId>,
}

pub(super) struct ActivationConfig {
    events: crossbeam_channel::Sender<ActivationEvent>,
    lifecycle_target: Option<MailboxId>,
}

impl ActivationConfig {
    pub(super) fn new(events: crossbeam_channel::Sender<ActivationEvent>) -> Self {
        Self { events, lifecycle_target: None }
    }

    pub(super) fn with_lifecycle_target(
        events: crossbeam_channel::Sender<ActivationEvent>,
        lifecycle_target: MailboxId,
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

    fn wire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
        if let Some(target) = state.lifecycle_target {
            let _ = ctx.send_envelope_detached(target, ActivationPoke::ID, &ActivationPoke.encode_into_bytes());
        }
        let _ = state.events.send(ActivationEvent::Wire(thread::current().id()));
    }

    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _poke: ActivationPoke) {
        let _ = self.events.send(ActivationEvent::Dispatch(thread::current().id()));
    }

    #[handler::single]
    fn on_close(&mut self, ctx: &mut NativeCtx<'_>, _close: ActivationClose) {
        let _ = self.events.send(ActivationEvent::Dispatch(thread::current().id()));
        ctx.shutdown();
    }

    fn unwire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
        if let Some(target) = state.lifecycle_target {
            let _ = ctx.send_envelope_detached(target, ActivationPoke::ID, &ActivationPoke.encode_into_bytes());
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
        Arc::new(ActorRegistry::new()),
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
    lifecycle_target: MailboxId,
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

pub(super) fn activation_sink(registry: &Registry, name: &str) -> (MailboxId, crossbeam_channel::Receiver<KindId>) {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let id = registry.register_inline(
        &boot_authority(),
        name,
        Arc::new(move |dispatch: MailDispatch<'_>| {
            let _ = sender.send(dispatch.kind);
        }),
    );
    (id, receiver)
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
    let deferred = parent.dispatch_arm::<SpawnOutcome, _>(
        spawner.mailer().acquire_settlement_hold(causing_chain),
        Source::NONE,
        (),
    );
    let dispatch_id = deferred.dispatch_id();
    let finalizer = NativeSpawnFinalizer::parented(
        parent_reservation,
        deferred,
        staged.identity.id,
        Arc::clone(&staged.identity.canonical_name),
        Arc::downgrade(&staged.transport),
        Arc::clone(spawner.mailer()),
    );

    (spawner.prepare_commit(staged, Some(finalizer), EffectChain::Held(causing_chain)), dispatch_id, key)
}

pub(super) fn await_spawn_done(parent: &NativeBinding, dispatch_id: DispatchId) -> TaskDone<SpawnOutcome, ()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(done) = parent.dispatch_take(dispatch_id) {
            return done;
        }
        assert!(Instant::now() < deadline, "native finalizer filled its typed deferred result");
        thread::yield_now();
    }
}
