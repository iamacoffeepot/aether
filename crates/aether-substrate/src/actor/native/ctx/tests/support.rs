//! Fixtures shared across the ctx test siblings: the stub actor the
//! type-level asserts stand on, the embedded peer the addressing tests send
//! at, the cast / request-context kinds they carry, and the booted
//! [`HeldHost`] whose handler turns arm the held replies the inbound and
//! registry tests follow.

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, mpsc};

use aether_actor::{Addressable, ErasedActorRef, HandlesKind, HeldReply, Manual};
use aether_data::{Kind, KindId, MailId};

use crate::actor::native::{
    Dispatch, DispatchId, Held, NativeActor, NativeCtx, NativeInitCtx, Pending, RegistryBatch, RegistryBatchResult,
    TaskDone,
};
use crate::chassis::builder::ReplyTarget;
use crate::chassis::error::BootError;
use crate::mail::mailer::Mailer;
use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
use crate::testing::{PumpedDriver, boot_test_chassis_with, fresh_substrate, registered_ref};

/// Hand-rolled `Addressable` impl referenced only by the `_assert_actor_send`
/// type-level check in [`super::handles`]. The struct never gets constructed
/// at runtime — its purpose is to fail to instantiate the assert if
/// `NativeActor` ever loses its `Send + 'static` bound.
#[allow(dead_code)]
pub(super) struct StubActor {
    boots: AtomicU32,
}

impl Addressable for StubActor {
    const NAMESPACE: &'static str = "test.stub";
    type Resolver = aether_actor::One;
}

impl aether_actor::Lifecycle<Self> for StubActor {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init((): (), _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { boots: AtomicU32::new(0) })
    }
}

impl Dispatch<Self> for StubActor {
    fn dispatch(
        _state: &mut Self,
        _ctx: &mut NativeCtx<'_, Self, Manual>,
        _kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        None
    }
}

impl aether_actor::Declared for StubActor {
    type Depends = ();
    type Spawns = ();
}
impl NativeActor for StubActor {
    type State = Self;
}

/// Issue 629 / Phase A: handle-export round-trip. Caps publish a
/// handle bundle during `init`; consumers retrieve a clone via
/// `get::<H>()`.
#[derive(Clone)]
pub(super) struct StubHandles {
    pub(super) counter: Arc<AtomicU32>,
}

/// A cast kind that is `Pod` but derives neither `Serialize` nor
/// `Deserialize` — the kind ADR-0100's reply path must accept.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct CastOnly {
    pub(super) code: u32,
}

impl Kind for CastOnly {
    const NAME: &'static str = "test.cast_only_reply";
    const ID: KindId = KindId(0xDEAD_BEEF_0009_0001);

    fn encode_into_bytes(&self) -> Vec<u8> {
        bytemuck::bytes_of(self).to_vec()
    }
}

impl aether_data::ActorMail for CastOnly {}
impl aether_data::CrossesActors for CastOnly {}

impl HandlesKind<CastOnly> for StubActor {}

#[aether_data::kind(name = "test.native_request_context", partial_eq)]
pub(super) struct NativeRequestContext {
    pub(super) value: u32,
}

/// The reply a [`HeldContext`]'s debt answers.
#[aether_data::kind(name = "test.native_held_reply", copy, partial_eq)]
pub(super) struct TestReply {
    pub(super) value: u32,
}

// A sentinel: no ctx test closes the actor holding a `TestReply` and reads
// the answer.
impl HeldReply for TestReply {
    fn unanswered() -> Self {
        Self { value: u32::MAX }
    }
}

/// A request context carrying a held reply (ADR-0243 §4), which parks in the
/// ledger when stored and comes back live when taken.
#[aether_data::kind(name = "test.native_held_context")]
pub(super) struct HeldContext {
    pub(super) held: Held<TestReply>,
    pub(super) tag: u32,
}

/// The request [`Bouncer`] answers with [`Poked`].
#[aether_data::kind(name = "test.native_held.poke")]
pub(super) struct Poke;

/// [`Bouncer`]'s answer, which lands back on [`HeldHost`] as the reply to a
/// request that stored a context.
#[aether_data::kind(name = "test.native_held.poked")]
pub(super) struct Poked;

/// Asks [`HeldHost`] to hold its reply and park it in a context carried on
/// a [`Poke`] to [`Bouncer`].
#[aether_data::kind(name = "test.native_held.park", copy)]
pub(super) struct ParkReq {
    pub(super) tag: u32,
}

/// Asks [`HeldHost`] to hold its reply and keep the ticket in state.
#[aether_data::kind(name = "test.native_held.hold")]
pub(super) struct HoldReq;

/// Asks [`HeldHost`] to hold its reply and stage a registry batch from it.
#[aether_data::kind(name = "test.native_held.stage")]
pub(super) struct StageReq;

/// A pooled root that answers every [`Poke`].
pub(super) struct Bouncer {
    pokes: u32,
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for Bouncer {
    const NAMESPACE: &'static str = "test.native_held.bouncer";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { pokes: 0 })
    }

    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _poke: Poke) -> Poked {
        self.pokes += 1;
        Poked
    }
}

/// What a [`HeldHost`] turn read off the real ledger through its ctx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LedgerRead {
    /// The held entry's state.
    pub(super) entry: Option<&'static str>,
    /// The holds open on the turn's chain.
    pub(super) held_open: u32,
}

impl LedgerRead {
    /// Read entry `id` and the holds open on `ctx`'s chain.
    fn at(ctx: &NativeCtx<'_, HeldHost>, id: DispatchId) -> Self {
        let root = ctx.in_flight_root().expect("the request runs on a tracked root");
        Self {
            entry: ctx.binding.dispatch_state_of(id),
            held_open: ctx.binding.mailer().trace_handle().settlement_counter().held_open(root),
        }
    }
}

/// A pumped root whose handler turns arm held replies (ADR-0243) and read
/// the real ledger through their ctx.
#[derive(Default)]
pub(super) struct HeldHost {
    /// Set by `on_hold`: a ticket kept in state for the test to move out.
    pub(super) held: Option<Held<TestReply>>,
    /// The held entry once `on_park` stored its context.
    pub(super) parked: Option<LedgerRead>,
    /// The context's tag and the entry's state once `on_poked` took it.
    pub(super) taken: Option<(u32, Option<&'static str>)>,
    /// The held entry once `on_stage` staged a batch from it.
    pub(super) staged: Option<LedgerRead>,
    /// The registry batches whose completion `on_batch_done` resolved.
    pub(super) batches: u32,
}

#[aether_actor::actor(singleton, root, depends(Bouncer))]
impl NativeActor for HeldHost {
    const NAMESPACE: &'static str = "test.native_held.host";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::single]
    fn on_park(&mut self, ctx: &mut NativeCtx<'_>, park: ParkReq) -> Pending<TestReply> {
        let (pending, held) = ctx.hold::<TestReply>();
        let id = held.dispatch_id();
        let _request = ctx.send_with_context::<Bouncer>(&Poke, HeldContext { held, tag: park.tag });
        self.parked = Some(LedgerRead::at(ctx, id));
        pending
    }

    #[handler::single]
    fn on_hold(&mut self, ctx: &mut NativeCtx<'_>, _hold: HoldReq) -> Pending<TestReply> {
        let (pending, held) = ctx.hold::<TestReply>();
        self.held = Some(held);
        pending
    }

    #[handler::single]
    fn on_poked(&mut self, ctx: &mut NativeCtx<'_>, _poked: Poked) {
        let HeldContext { held, tag } = ctx.take_context::<HeldContext>().expect("the reply takes its stored context");
        self.taken = Some((tag, ctx.binding.dispatch_state_of(held.dispatch_id())));
        held.answer(ctx, &TestReply { value: tag });
    }

    #[handler::single]
    fn on_stage(&mut self, ctx: &mut NativeCtx<'_>, _stage: StageReq) -> Pending<TestReply> {
        let (pending, held) = ctx.hold::<TestReply>();
        let id = held.dispatch_id();
        let _batch = ctx.stage_registry_batch_from(held, RegistryBatch::register_kinds(Vec::new()), ());
        self.staged = Some(LedgerRead::at(ctx, id));
        pending
    }

    #[handler(task)]
    fn on_batch_done(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<RegistryBatchResult>) {
        self.batches += 1;
        done.resolve_value(ctx, &TestReply { value: 5 });
    }
}

/// A booted [`HeldHost`] beside its pooled [`Bouncer`], and the mailer its
/// chains settle through.
pub(super) struct HeldRig {
    pub(super) driver: PumpedDriver<HeldHost>,
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
}

impl HeldRig {
    pub(super) fn boot() -> Self {
        let (registry, mailer) = fresh_substrate();
        let driver = PumpedDriver::boot(boot_test_chassis_with::<Bouncer>(&registry, &mailer, (), ()), (), ());

        Self { driver, registry, mailer }
    }

    /// A caller registered under `name` that forwards each reply it receives
    /// for the test to read, then finishes it, so the chain the reply joined
    /// settles only once the reply is readable.
    pub(super) fn caller(&self, name: &str) -> (ErasedActorRef, mpsc::Receiver<OwnedDispatch>) {
        let (tx, rx) = mpsc::channel::<OwnedDispatch>();
        let mailer = Arc::clone(&self.mailer);
        let sink: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            let (mail_id, root) = (dispatch.mail_id, dispatch.root);
            dispatch.discharge();
            let _ = tx.send(dispatch);
            mailer.record_finished(mail_id, root);
        });

        (registered_ref(&self.registry, name, sink), rx)
    }

    /// Push `mail` to the host as a tracked root, answered to `reply_to`
    /// under correlation 77 when given.
    pub(super) fn push<K: Kind>(&self, mail: &K, reply_to: Option<ErasedActorRef>) -> MailId
    where
        HeldHost: HandlesKind<K>,
    {
        let reply = reply_to.map(|to| ReplyTarget::Actor { to, correlation: 77 });
        self.driver.send_tracked(self.driver.chassis().actor_ref::<HeldHost>(), mail, reply)
    }

    pub(super) fn held_open(&self, root: MailId) -> u32 {
        self.mailer.trace_handle().settlement_counter().held_open(root)
    }
}
