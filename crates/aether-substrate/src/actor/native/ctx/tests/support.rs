//! Fixtures shared across the ctx test siblings: the stub actor the
//! type-level asserts stand on, the cast / request-context kinds the tests
//! carry, the finishing [`sink`] their sends land in, the booted [`Reader`]
//! whose turns read the sender a real dispatch stamps and whose host turns
//! run the verbs that need no inbound, the pooled [`Pinger`] that knocks on
//! it, and the booted [`HeldHost`] whose handler turns arm the held replies
//! the inbound and registry tests follow.

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, mpsc};

use aether_actor::{ActorRef, Addressable, ErasedActorRef, HandlesKind, HeldReply, Unchecked};
use aether_data::{ErasedActorPath, Kind, KindId, MailId};

use crate::actor::native::envelope::Envelope;
use crate::actor::native::{Dispatch, DispatchId, Held, NativeActor, NativeCtx, NativeInitCtx, Pending, Subname};
use crate::chassis::builder::ReplyTarget;
use crate::chassis::error::BootError;
use crate::mail::mailer::Mailer;
use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
use crate::testing::{
    PumpedDriver, bare_substrate, boot_bare_test_chassis, boot_test_chassis_with, fresh_substrate, registered_ref,
};

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
        _ctx: &mut NativeCtx<'_, Self, Unchecked>,
        _kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        None
    }
}

impl aether_actor::Declared for StubActor {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
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

impl HandlesKind<CastOnly> for StubActor {
    type Sender = aether_actor::Anyone;
}

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

    #[handler::request]
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
}

#[aether_actor::actor(singleton, root, depends(Bouncer))]
impl NativeActor for HeldHost {
    const NAMESPACE: &'static str = "test.native_held.host";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::request]
    fn on_park(&mut self, ctx: &mut NativeCtx<'_>, park: ParkReq) -> Pending<TestReply> {
        let (pending, held) = ctx.hold::<TestReply>();
        let id = held.dispatch_id();
        let _request = ctx.send_with_context::<Bouncer>(&Poke, HeldContext { held, tag: park.tag });
        self.parked = Some(LedgerRead::at(ctx, id));
        pending
    }

    #[handler::request]
    fn on_hold(&mut self, ctx: &mut NativeCtx<'_>, _hold: HoldReq) -> Pending<TestReply> {
        let (pending, held) = ctx.hold::<TestReply>();
        self.held = Some(held);
        pending
    }

    #[handler::response]
    fn on_poked(&mut self, ctx: &mut NativeCtx<'_>, _poked: Poked, HeldContext { held, tag }: HeldContext) {
        self.taken = Some((tag, ctx.binding.dispatch_state_of(held.dispatch_id())));
        held.answer(ctx, &TestReply { value: tag });
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

    /// A caller registered under `name` that hands each reply it receives to
    /// the test, as a [`sink`] does.
    pub(super) fn caller(&self, name: &str) -> (ErasedActorRef, mpsc::Receiver<Envelope>) {
        sink(&self.registry, &self.mailer, name)
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

/// An inbox registered under `name` that hands each envelope it receives to
/// the returned receiver, then finishes it, so the chain the envelope joined
/// settles only once the test can read it.
pub(super) fn sink(
    registry: &Registry,
    mailer: &Arc<Mailer>,
    name: &str,
) -> (ErasedActorRef, mpsc::Receiver<Envelope>) {
    let (tx, rx) = mpsc::channel::<Envelope>();
    let mailer = Arc::clone(mailer);
    let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
        let (mail_id, root) = (dispatch.mail_id, dispatch.root);
        // Terminal test sink (ADR-0094): discharge before observing.
        dispatch.discharge();
        let _ = tx.send(dispatch);
        mailer.record_finished(mail_id, root);
    });

    (registered_ref(registry, name, handler), rx)
}

/// The mail a [`Reader`] turn reads its sender off.
#[aether_data::kind(name = "test.native_ctx.knock")]
pub(super) struct Knock;

/// Asks a [`Pinger`] to send its reader a [`Knock`].
#[aether_data::kind(name = "test.native_ctx.ping")]
pub(super) struct Ping;

/// Asks a [`Pinger`] to shut down.
#[aether_data::kind(name = "test.native_ctx.leave")]
pub(super) struct Leave;

/// A pumped root whose [`Knock`] turns record the sender their ctx proves and
/// the path it names, and whose host turns run the verbs that read no inbound.
#[derive(Default)]
pub(super) struct Reader {
    /// One entry per [`Knock`] turn, in arrival order.
    pub(super) senders: Vec<Option<(ErasedActorRef, ErasedActorPath)>>,
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for Reader {
    const NAMESPACE: &'static str = "test.native_ctx.reader";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::tell]
    fn on_knock(&mut self, ctx: &mut NativeCtx<'_>, _knock: Knock) {
        self.senders.push(ctx.sender().map(|sender| (sender, ctx.actor_path(sender))));
    }
}

/// A pooled peer that knocks on the reader it was spawned with, so the
/// [`Knock`] carries the sender stamp a real send writes.
pub(super) struct Pinger {
    reader: ActorRef<Reader>,
}

#[aether_actor::actor(instanced)]
impl NativeActor for Pinger {
    const NAMESPACE: &'static str = "test.native_ctx.pinger";
    type Config = ();
    type Params = ActorRef<Reader>;

    fn init((): (), reader: ActorRef<Reader>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { reader })
    }

    #[handler::tell]
    fn on_ping(&mut self, ctx: &mut NativeCtx<'_>, _ping: Ping) {
        ctx.send_to(self.reader, &Knock);
    }

    #[handler::tell]
    fn on_leave(&mut self, ctx: &mut NativeCtx<'_>, _leave: Leave) {
        let _ = self;
        ctx.shutdown();
    }
}

/// A booted [`Reader`] and the substrate its [`Pinger`]s are spawned on.
pub(super) struct ReaderRig {
    pub(super) driver: PumpedDriver<Reader>,
    pub(super) registry: Arc<Registry>,
    pub(super) mailer: Arc<Mailer>,
}

impl ReaderRig {
    pub(super) fn boot() -> Self {
        let (registry, mailer) = bare_substrate();
        let driver = PumpedDriver::boot(boot_bare_test_chassis(&registry, &mailer), (), ());

        Self { driver, registry, mailer }
    }

    /// A [`Pinger`] spawned under `key`, knocking on this rig's reader.
    pub(super) fn pinger(&self, key: &str) -> ActorRef<Pinger> {
        let chassis = self.driver.chassis();
        chassis
            .spawn_actor_for_test::<Pinger>(Subname::Named(key), (), chassis.actor_ref::<Reader>())
            .finish()
            .expect("the pinger spawns")
    }

    /// Have `pinger` knock, and settle the chain that carries the knock.
    pub(super) fn ping(&mut self, pinger: ActorRef<Pinger>) {
        let root = self.driver.send_tracked(pinger, &Ping, None);
        self.driver.settle(&[root]);
    }

    /// Push a [`Knock`] to the reader as a chassis root, answered to `reply`
    /// when given, and settle it.
    pub(super) fn knock(&mut self, reply: Option<ErasedActorRef>) {
        let reader = self.driver.chassis().actor_ref::<Reader>();
        self.driver.send_and_settle(reader, &Knock, reply.map(|to| ReplyTarget::Actor { to, correlation: 0 }));
    }

    /// Shut `pinger` down and wait until its route has retired.
    pub(super) fn close(&mut self, pinger: ActorRef<Pinger>) {
        let _ = self.driver.send_tracked(pinger, &Leave, None);
        self.driver.chassis().await_closed(pinger.erase());
    }

    /// What every [`Knock`] turn so far read.
    pub(super) fn senders(&self) -> Vec<Option<(ErasedActorRef, ErasedActorPath)>> {
        self.driver.read_state(|reader| reader.senders.clone()).expect("the reader is live")
    }
}
