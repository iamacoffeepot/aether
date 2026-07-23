//! [`PumpedSlot<A>`] — the externally-pumped dispatch home (ADR-0160 §1).
//!
//! A `DispatcherSlot` drains on the chassis worker pool; a `PumpedSlot`
//! drains on the thread a chassis driver hands it, at a pump point the driver
//! chooses (the desktop window driver drains in `about_to_wait`). It is a
//! strict subset of the pooled slot: a single pump thread means there is no
//! `SlotState` machine, no actor mutex, and no seize path — the slot owns its
//! actor outright. It is deliberately **not `Send`**: it lives and dies on
//! the pumping thread.
//!
//! The per-envelope dispatch body and the Phase-4 registry close are the
//! **same** free functions the pooled slot runs (`dispatch_envelope` and
//! `finalize_registry_and_fan_out`), so `describe`, trace hops, and
//! `actor_cost` behave identically across the two homes — the drift a
//! hand-rolled driver drain would accrue is structurally impossible.
//!
//! [`PumpedSlot::drain_available`] drains every queued envelope; it is
//! callable from any pump point on the owning thread.
//! [`PumpedSlot::host_turn`] gives that thread bounded mutable host ingress
//! under the actor's stamped context without draining queued mail.
//! [`PumpedSlot::shutdown`] runs the pooled Closed path's phases in order:
//! residual drain, `unwire` under `with_stamped`, cost-row drop
//! (iamacoffeepot/aether#3051), and the registry close + monitor fan-out.

use core::marker::PhantomData;
use std::sync::Arc;

use aether_actor::local::ActorSlots;

use crate::actor::native::NativeActor;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::dispatcher_slot::{dispatch_envelope, finalize_registry_and_fan_out};
use crate::actor::native::local;
use crate::actor::registry::ActorRegistry;
use crate::mail::{MailboxId, Source};
use aether_data::MailId;

/// The externally-pumped dispatch home for a native actor (ADR-0160 §1).
/// See the [module docs](self) for how it relates to the pooled
/// `DispatcherSlot`.
pub struct PumpedSlot<A>
where
    A: NativeActor,
{
    /// The actor itself, owned outright — no mutex, because one pump thread
    /// is the sole accessor. `Option` so [`Self::shutdown`] can take the box
    /// out to run `unwire` on it and mark the slot spent (a second
    /// `shutdown` / a post-shutdown `drain_available` is then a no-op).
    actor: Option<Box<A::State>>,
    /// Per-actor binding (inbox + reply machinery + outbound buffer). The
    /// pump reaches the inbox through [`NativeBinding::try_recv`].
    binding: Arc<NativeBinding>,
    /// Per-actor `Local<T>` storage, stamped into TLS around each dispatch
    /// and around `unwire`. A plain `Box<ActorSlots>` rather than the pooled
    /// slot's `Sync`-wrapped `PooledSlots`: a pumped slot is single-threaded,
    /// so the interior `RefCell` never races.
    slots: Box<ActorSlots>,
    /// Chassis-level actor registry — drained + pruned by the Phase-4 close
    /// on [`Self::shutdown`].
    actor_registry: Arc<ActorRegistry>,
    /// This slot's mailbox id — passed to the cost-table drop and the
    /// registry close.
    self_id: MailboxId,
    /// Deliberately `!Send` / `!Sync` (ADR-0160 §1): the slot is owned by
    /// one pump thread and must not cross to another. Nothing in the real
    /// fields forbids `Send` on its own (the slots box is `Send`), so this
    /// marker pins the invariant into the type.
    _not_send: PhantomData<*const ()>,
}

impl<A> PumpedSlot<A>
where
    A: NativeActor,
{
    /// Assemble a pumped slot from its already-booted parts. The parts are
    /// produced by
    /// [`DriverCtx::boot_pumped_actor`](crate::chassis::builder::DriverCtx::boot_pumped_actor),
    /// which recovers the driver's Claim-stage reservation, builds the
    /// binding, seeds the slots' rings + cost cache, and runs `init` / `wire`
    /// — so by the time a slot exists it is a fully-wired actor ready to
    /// pump.
    pub(crate) fn new(
        actor: Box<A::State>,
        binding: Arc<NativeBinding>,
        slots: Box<ActorSlots>,
        actor_registry: Arc<ActorRegistry>,
        self_id: MailboxId,
    ) -> Self {
        Self { actor: Some(actor), binding, slots, actor_registry, self_id, _not_send: PhantomData }
    }

    /// Drain every envelope currently queued on the actor's inbox, running
    /// the shared `dispatch_envelope` body for each. Callable from any pump
    /// point on the owning thread (the desktop driver calls it in
    /// `about_to_wait`). A no-op once [`Self::shutdown`] has consumed the
    /// actor.
    pub fn drain_available(&mut self) {
        let Some(actor) = self.actor.as_mut() else {
            return;
        };
        while let Some(env) = self.binding.try_recv() {
            dispatch_envelope::<A>(actor, &self.binding, &self.slots, env);
        }
    }

    /// Run one bounded host-originated turn against this actor's mutable
    /// state on the pumping thread.
    ///
    /// The turn is stamped with this actor's [`ActorSlots`] and receives an
    /// inbound-less [`NativeCtx`]. Outbound mail therefore starts fresh
    /// causal roots and flushes through the normal context-drop path before
    /// the stamp is removed. This does not drain the inbox or dispatch
    /// self-mail; callers retain explicit control over pump ordering through
    /// [`Self::drain_available`].
    ///
    /// Returns `None` without invoking `turn` once [`Self::shutdown`] has
    /// consumed the actor.
    pub fn host_turn<R>(&mut self, turn: impl FnOnce(&mut A::State, &mut NativeCtx<'_>) -> R) -> Option<R> {
        let actor = self.actor.as_deref_mut()?;
        let binding = &self.binding;
        let slots = &self.slots;
        Some(local::with_stamped(slots, || {
            let mut ctx = NativeCtx::new(binding, Source::NONE, MailId::NONE, MailId::NONE);
            turn(actor, &mut ctx)
        }))
    }

    /// Read from the actor's `A::State` without mutating it (ADR-0161
    /// §Decision 1). The pump-owning thread makes loop decisions off actor
    /// state — the render driver reads its pending capture's deadline to set
    /// `ControlFlow::WaitUntil` — and this is the only such access it gets:
    /// `f` receives a shared `&A::State`, so no `&mut` escapes and everything
    /// that mutates the actor still goes through a pumped handler. Returns
    /// `None` once [`Self::shutdown`] has taken the actor out (the slot is
    /// spent), otherwise `Some(f(&state))`.
    pub fn read_state<R>(&self, f: impl FnOnce(&A::State) -> R) -> Option<R> {
        self.actor.as_deref().map(f)
    }

    /// Run the pooled Closed path's teardown phases on the pumping thread,
    /// in order: drain any residual inbox mail, run `A::unwire` under
    /// `with_stamped` (the hook a hand-rolled driver drain never had), drop
    /// the finalized mailbox's cost rows (iamacoffeepot/aether#3051), and
    /// run the registry close + monitor fan-out. Idempotent — the actor is
    /// taken out on the first call, so a second `shutdown` (or any later
    /// `drain_available`) is a no-op.
    pub fn shutdown(&mut self) {
        let Some(mut actor) = self.actor.take() else {
            return;
        };
        // Phase 2: drain residual inbox synchronously.
        while let Some(env) = self.binding.try_recv() {
            dispatch_envelope::<A>(&mut actor, &self.binding, &self.slots, env);
        }
        // Phase 3: the `unwire` hook, under this actor's stamped slots so any
        // final `tracing::*` / `Local<T>` access resolves to its rings.
        local::with_stamped(&self.slots, || {
            let mut close_ctx = NativeCtx::new(&self.binding, Source::NONE, MailId::NONE, MailId::NONE);
            A::unwire(actor.as_mut(), &mut close_ctx);
        });
        // iamacoffeepot/aether#3051: the close hook is the last phase allowed
        // to observe this actor's handler costs; drop its global rows now so
        // native instance churn can't retain stale cells.
        self.binding.mailer().cost_table().drop_mailbox(self.self_id);
        // Phase 4: registry close + monitor fan-out.
        finalize_registry_and_fan_out(&self.actor_registry, self.binding.mailer(), self.self_id);
        // `actor` drops here — the box was taken out of the `Option`, so the
        // slot is now spent.
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
// Test fixtures derive mailbox ids by name and spin worker threads that hold
// no settlement contract; both trip the disallowed-methods lint by design.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use aether_actor::local::ActorSlots;
    use aether_actor::log::ActorLogRing;
    use aether_actor::trace::ActorTraceRing;
    use aether_actor::{Addressable, HandlesKind, Local as _, MailSender, Manual, One};
    use aether_data::{Kind, KindId, MailId, MailboxId, Schema, Source, SourceAddr, mailbox_id_from_name};
    use aether_kinds::trace::TraceEvent;
    use aether_kinds::{CostTail, CostTailResult, LogTail, LogTailResult, descriptors};

    use crate::actor::native::Dispatch;
    use crate::actor::native::envelope::Envelope;
    use crate::actor::native::local::with_stamped;
    use crate::actor::registry::ActorRegistry;
    use crate::chassis::inbox::{InboundMail, ReplyLineage, SettlingInbox};
    use crate::chassis::settlement::{
        PumpWake, SettlementRegistry, TerminalDisposition, WaitOutcome, await_settlement_pumped,
    };
    use crate::config::RingCapacities;
    use crate::mail::Mail;
    use crate::mail::cost::CostCells;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::{InboxHandler, Registry};
    use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
    use crate::scheduler::{Pool, PoolConfig, PoolHandle};
    use crate::{BootError, NativeInitCtx};

    #[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize, Kind, Schema)]
    #[kind(name = "test.pumped.ping")]
    struct Ping {
        seq: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize, Kind, Schema)]
    #[kind(name = "test.pumped.pong")]
    struct Pong {
        seq: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize, Kind, Schema)]
    #[kind(name = "test.pumped.defer")]
    struct Defer {
        seq: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize, Kind, Schema)]
    #[kind(name = "test.pumped.emit")]
    struct EmitReq {
        seq: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize, Kind, Schema)]
    #[kind(name = "test.pumped.poke")]
    struct Poke {
        note: u32,
    }

    /// A pure addressing identity for the peer `on_emit` sends to (test 6).
    /// `One` makes it a root singleton, so `Peer::resolve(_, ())` equals
    /// `mailbox_id_from_name("test.pumped.peer")` regardless of the caller's
    /// carry.
    struct Peer;
    impl Addressable for Peer {
        const NAMESPACE: &'static str = "test.pumped.peer";
        type Resolver = One;
    }
    impl HandlesKind<Poke> for Peer {}

    /// The toy actor the tests pump by hand. Its handlers exercise every
    /// dispatch path a pumped slot runs: a `-> R` reply, the framework
    /// `log.tail` arm (served for a kind it does not handle), the cost fold,
    /// a deferred (retained-inbound) reply, and a non-reply peer send.
    #[derive(Default)]
    struct PumpProbe {
        /// Set by `on_ping` — proves the typed handler ran (not asserted
        /// directly, but keeps the handler `&mut self`).
        pings: u32,
        /// When present, `unwire` flips it — the test 4 observable that
        /// `shutdown` ran the close hook after the actor is gone.
        unwired: Option<Arc<AtomicBool>>,
        /// When present, `on_defer` ships its retained inbound guard here so
        /// the test replies from a worker thread (test 5).
        deferred_tx: Option<mpsc::Sender<InboundMail>>,
    }

    #[aether_actor::actor]
    impl NativeActor for PumpProbe {
        const NAMESPACE: &'static str = "test.pumped.probe";
        type Config = ();

        fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self::default())
        }

        #[handler::single]
        fn on_ping(&mut self, _ctx: &mut NativeCtx<'_>, req: Ping) -> Pong {
            self.pings += 1;
            Pong { seq: req.seq }
        }

        #[handler::manual]
        fn on_defer(&mut self, ctx: &mut NativeCtx<'_, Manual>, _d: Defer) {
            if let Some(tx) = &self.deferred_tx {
                // Retain the inbound past this handler's return; the reply is
                // sent from a worker thread and the guard settles the chain
                // on drop (ADR-0106).
                let _ = tx.send(ctx.take_inbound());
            }
        }

        #[handler::single]
        #[allow(clippy::unused_self)]
        fn on_emit(&mut self, ctx: &mut NativeCtx<'_>, _e: EmitReq) {
            // A non-reply peer send: buffered here, flushed at ctx drop
            // through the binding's outbound blob → the pool `WakeSink`,
            // exercising the pool-side blob demux from the pumping thread.
            ctx.actor::<Peer>().send(&Poke { note: 7 });
        }

        fn unwire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            if let Some(flag) = &state.unwired {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Long-lived owned infra a set of pumped-slot tests share.
    struct Fixtures {
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
        settlement: Arc<SettlementRegistry>,
        actor_registry: Arc<ActorRegistry>,
        spawner: Arc<crate::Spawner>,
        aborter: Arc<dyn FatalAborter>,
        _pool: PoolHandle,
    }

    fn fixtures() -> Fixtures {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        // Wire one settlement registry into both seams — the chassis builder
        // does both installs at boot — so armed Calls drain cleanly.
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));

        let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
        let actor_registry = Arc::new(ActorRegistry::new());
        let pool = Pool::start(PoolConfig::default(), Arc::clone(&aborter));
        let spawner = Arc::new(crate::Spawner::new(
            Arc::clone(&registry),
            Arc::clone(&actor_registry),
            Arc::clone(&mailer),
            Arc::clone(&aborter),
            pool.wake_sink(),
            RingCapacities::default(),
        ));
        Fixtures { registry, mailer, settlement, actor_registry, spawner, aborter, _pool: pool }
    }

    /// Seed the pumped slot's log / trace rings and stamp the actor's
    /// per-handler cost cells into `slots` — the exact per-actor seeding
    /// `NativeActorBoot` / `boot_pumped_actor` run, so the framework arms and
    /// the cost fold resolve identically under test.
    fn seed_slots(mailer: &Arc<Mailer>, self_id: MailboxId, slots: &ActorSlots) {
        let caps = RingCapacities::default();
        slots.seed(ActorLogRing::with_capacity(caps.log));
        slots.seed(ActorTraceRing::with_growth(caps.trace, caps.trace_max));

        let handler_kinds: Vec<KindId> =
            <PumpProbe as Dispatch<PumpProbe>>::capabilities().handlers.iter().map(|h| h.id).collect();
        let seeded = mailer.cost_table().seed(self_id, &handler_kinds);
        with_stamped(slots, || {
            use aether_actor::Local as _;
            CostCells::try_with_mut(|cells| cells.seed(seeded));
        });
    }

    /// Register the pumped mailbox (forwarding armed envelopes onto the
    /// binding's inbox channel, exactly as `claim_mailbox` does), build a
    /// spawner-backed binding, install a seeded slots box, and assemble the
    /// `PumpedSlot`. `settling` chooses the install path — `false` builds a
    /// fresh inbox via `install_inbox`, `true` installs a claim-shaped
    /// `SettlingInbox` re-lineaged onto the binding's reply space (the
    /// `boot_pumped_actor` path).
    ///
    /// `wake_tx`, when present, is fired with [`PumpWake::Mail`] after each
    /// accepted inbound send — mirroring the production `MailboxWakeSlot` hook
    /// `install_pump_wake` installs — so a slot pumped through
    /// `await_settlement_pumped` wakes on mail arrival.
    fn boot_probe(
        fx: &Fixtures,
        self_id: MailboxId,
        actor: PumpProbe,
        settling: bool,
        wake_tx: Option<crossbeam_channel::Sender<PumpWake>>,
    ) -> PumpedSlot<PumpProbe> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: Envelope| {
            let _ = tx.send(d);
            if let Some(wake_tx) = &wake_tx {
                let _ = wake_tx.send(PumpWake::Mail);
            }
        });
        fx.registry
            .try_register_inbox_with_id(self_id, "test.pumped.probe", handler)
            .expect("register the pumped mailbox");

        let binding = Arc::new(NativeBinding::new(
            Arc::clone(&fx.mailer),
            self_id,
            self_id.0,
            Arc::clone(&fx.aborter),
            Some(Arc::clone(&fx.spawner)),
        ));
        if settling {
            let inbox = SettlingInbox::new(self_id, rx, Arc::clone(&fx.mailer));
            binding.install_settling_inbox(inbox.relineage(binding.reply_lineage()));
        } else {
            binding.install_inbox(rx);
        }

        let slots = Box::new(ActorSlots::new());
        seed_slots(&fx.mailer, self_id, &slots);
        PumpedSlot::new(Box::new(actor), binding, slots, Arc::clone(&fx.actor_registry), self_id)
    }

    /// Register a `Component` inbox that discharges each armed reply and
    /// forwards it onto `tx` so a test can read the delivered envelope.
    fn caller_inbox(fx: &Fixtures, name: &str) -> (MailboxId, mpsc::Receiver<Envelope>) {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let id = fx.registry.register_inbox(
            name.to_owned(),
            Arc::new(move |d: Envelope| {
                d.discharge();
                let _ = tx.send(d);
            }) as Arc<dyn InboxHandler>,
        );
        (id, rx)
    }

    /// ADR-0160 §1 (re-homes `try_framework_dispatch_replies_to_log_tail` /
    /// `return_type_handler_replies_through_the_macro`): a mail dispatched
    /// through `drain_available` reaches its `-> R` handler and the reply
    /// carries the inbound's `root`, joining the caller's ADR-0080 chain —
    /// the handler-return reply path runs identically to the pooled slot.
    #[test]
    fn drain_available_reply_carries_inbound_root() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0001);
        let (caller, reply_rx) = caller_inbox(&fx, "test.pumped.caller.ping");
        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, None);

        let root = MailId::new(self_id, 1);
        let mail_id = MailId::new(self_id, 2);
        fx.mailer.record_sent_inflight(root);
        let caller_source = Source::with_correlation(SourceAddr::Component(caller), 0x99);
        let bytes = Ping { seq: 7 }.encode_into_bytes();
        fx.mailer.push(
            Mail::new(self_id, Ping::ID, bytes, 1).with_reply_to(caller_source).with_lineage(mail_id, root, None),
        );

        slot.drain_available();

        let reply = reply_rx.recv_timeout(Duration::from_secs(2)).expect("the -> Pong reply routed to the caller");
        assert_eq!(reply.kind, Pong::ID, "the reply carries the handler's declared return kind");
        assert_eq!(reply.root, root, "the reply joins the inbound's causal chain");
        let pong = Pong::decode_from_bytes(reply.payload.bytes()).expect("reply decodes");
        assert_eq!(pong, Pong { seq: 7 }, "the value the handler returned is what was replied");

        // Finish the reply so the chain settles and bookkeeping balances.
        fx.mailer.record_finished(reply.mail_id, root);
    }

    /// ADR-0160 §1 (re-homes the desktop framework-arm test): a `LogTail`
    /// Call the toy actor does not handle is served by the framework
    /// `aether.log.tail` arm inside the pumped drain, replying a
    /// `LogTailResult` on the inbound's chain. The reply appearing proves
    /// the framework arm fired — a strict receiver would otherwise warn-drop
    /// the kind.
    #[test]
    fn log_tail_served_by_framework_arm() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0002);
        let (caller, reply_rx) = caller_inbox(&fx, "test.pumped.caller.logtail");
        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, None);

        let root = MailId::new(self_id, 1);
        let mail_id = MailId::new(self_id, 2);
        fx.mailer.record_sent_inflight(root);
        let caller_source = Source::with_correlation(SourceAddr::Component(caller), 0x33);
        let bytes = LogTail { max: 8, min_level: None, since: None, contains: None }.encode_into_bytes();
        fx.mailer.push(
            Mail::new(self_id, <LogTail as Kind>::ID, bytes, 1)
                .with_reply_to(caller_source)
                .with_lineage(mail_id, root, None),
        );

        slot.drain_available();

        let reply = reply_rx.recv_timeout(Duration::from_secs(2)).expect("the framework arm replied to the caller");
        assert_eq!(reply.kind, <LogTailResult as Kind>::ID, "the framework arm replied a LogTailResult");
        assert_eq!(reply.root, root, "the framework-arm reply joins the inbound's chain");
        fx.mailer.record_finished(reply.mail_id, root);
    }

    /// ADR-0160 §1 tripwire: two pumped drains fold two cost samples into
    /// the handler's per-mailbox EWMA row — the fold a hand-rolled driver
    /// drain never ran. If `drain_available` stopped routing through the
    /// shared `dispatch_envelope`, this row would stay at zero samples.
    #[test]
    fn two_drains_fold_cost_samples() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0003);
        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, None);

        // Two disarmed pings (NONE lineage — no reply target, no settlement
        // to balance); the run's only observable is the cost fold.
        for _ in 0..2 {
            let bytes = Ping { seq: 1 }.encode_into_bytes();
            fx.mailer.push(Mail::new(self_id, Ping::ID, bytes, 1).with_lineage(MailId::NONE, MailId::NONE, None));
        }

        slot.drain_available();

        let CostTailResult::Ok { rows } = fx.mailer.cost_table().tail(self_id, &CostTail { kind: Some(Ping::ID) })
        else {
            panic!("expected Ok");
        };
        let row = rows.iter().find(|r| r.kind_id == Ping::ID).expect("the Ping handler's cost row is present");
        assert_eq!(row.samples, 2, "each pumped drain folds one handler-cost sample");
    }

    /// ADR-0160 §1 (re-homes `window_inbox_drain_settles_root_on_guard_drop`):
    /// `shutdown` runs the pooled Closed path's phases — it drains residual
    /// inbox mail (settling its root), runs `unwire`, and drops the
    /// finalized mailbox's cost rows — and is idempotent on a second call.
    #[test]
    fn shutdown_runs_unwire_settles_residual_and_drops_cost() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0004);
        let unwired = Arc::new(AtomicBool::new(false));
        let mut slot = boot_probe(
            &fx,
            self_id,
            PumpProbe { unwired: Some(Arc::clone(&unwired)), ..Default::default() },
            false,
            None,
        );

        // An armed residual mail queued but never handed to `drain_available`
        // — `shutdown`'s residual drain must dispatch it and settle its root.
        let root = MailId::new(self_id, 1);
        let mail_id = MailId::new(self_id, 2);
        fx.mailer.record_sent_inflight(root);
        let settle = fx.settlement.subscribe_settlement(root);
        let bytes = Ping { seq: 5 }.encode_into_bytes();
        fx.mailer.push(Mail::new(self_id, Ping::ID, bytes, 1).with_lineage(mail_id, root, None));

        slot.shutdown();

        assert!(unwired.load(Ordering::SeqCst), "shutdown ran the unwire hook");
        settle.recv().expect("shutdown's residual drain settled the queued root");
        let CostTailResult::Ok { rows } = fx.mailer.cost_table().tail(self_id, &CostTail { kind: None }) else {
            panic!("expected Ok");
        };
        assert!(rows.is_empty(), "shutdown dropped the finalized mailbox's cost rows");

        // Idempotent: the actor was consumed on the first call.
        slot.shutdown();
    }

    /// ADR-0160 §1: a handler that retains its inbound (`take_inbound`) and
    /// replies from a worker thread mints the deferred reply's id in the
    /// binding's disjoint reply-lineage space, stamped with the pumped
    /// mailbox — the relineage invariant holds for deferred replies, not
    /// just synchronous ones. Exercises `install_settling_inbox` +
    /// `SettlingInbox::relineage` (the boot install path) and the retained
    /// guard's cross-thread settle-exactly-once.
    #[test]
    fn deferred_reply_mints_in_binding_reply_space() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0005);
        let (target, reply_rx) = caller_inbox(&fx, "test.pumped.caller.defer");
        let (guard_tx, guard_rx) = mpsc::channel::<InboundMail>();
        let mut slot =
            boot_probe(&fx, self_id, PumpProbe { deferred_tx: Some(guard_tx), ..Default::default() }, true, None);

        let root = MailId::new(self_id, 1);
        let mail_id = MailId::new(self_id, 2);
        fx.mailer.record_sent_inflight(root);
        let settle = fx.settlement.subscribe_settlement(root);
        let sender = Source::with_correlation(SourceAddr::Component(target), 7);
        let bytes = Defer { seq: 1 }.encode_into_bytes();
        fx.mailer.push(Mail::new(self_id, Defer::ID, bytes, 1).with_reply_to(sender).with_lineage(mail_id, root, None));

        slot.drain_available();

        // The handler deferred: it retained the inbound and shipped the guard
        // out. The retained guard holds the chain open (no premature settle).
        let guard = guard_rx.recv_timeout(Duration::from_secs(2)).expect("the manual handler deferred the inbound");
        assert!(settle.try_recv().is_err(), "the retained guard holds the chain open — no premature settle");

        // Reply + settle the inbound from a worker thread (the deferred-reply
        // shape). `InboundMail: Send`, so the guard crosses.
        let worker = thread::spawn(move || {
            assert!(guard.reply(&Pong { seq: 9 }), "the deferred reply routed to the Component target");
            drop(guard);
        });
        worker.join().expect("worker thread joins");

        let reply = reply_rx.recv().expect("the deferred reply routed to the target inbox");
        assert!(
            reply.mail_id.correlation_id >= ReplyLineage::BASE,
            "the deferred reply mints in the disjoint reply-lineage space",
        );
        assert_eq!(reply.mail_id.sender, self_id, "the deferred reply id is stamped with the pumped mailbox");

        // The reply's Sent held the chain open; finishing it settles the root
        // exactly once.
        assert!(settle.try_recv().is_err(), "the reply's Sent still holds the chain open");
        fx.mailer.record_finished(reply.mail_id, root);
        settle.recv().expect("the root settles once the deferred reply finishes");
    }

    /// ADR-0160 §1: a non-reply `ctx.actor::<Peer>().send(...)` from a pumped
    /// handler routes through the binding's outbound blob → the pool
    /// `WakeSink` (the pool-side blob demux) from the pumping thread, and the
    /// peer receives the mail. Dormant for the window driver, load-bearing
    /// for the render follow-up — so the path must be solid before that arc.
    #[test]
    fn pumped_handler_send_reaches_peer_through_blob_demux() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0006);

        // Register the peer inbox at its resolved (carry-independent) id.
        let peer_id = mailbox_id_from_name(Peer::NAMESPACE);
        let (poke_tx, poke_rx) = mpsc::channel::<Envelope>();
        fx.registry
            .try_register_inbox_with_id(
                peer_id,
                "test.pumped.peer",
                Arc::new(move |d: Envelope| {
                    d.discharge();
                    let _ = poke_tx.send(d);
                }) as Arc<dyn InboxHandler>,
            )
            .expect("register the peer inbox");

        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, None);

        // A disarmed EmitReq (NONE lineage) triggers the peer send.
        let bytes = EmitReq { seq: 1 }.encode_into_bytes();
        fx.mailer.push(Mail::new(self_id, EmitReq::ID, bytes, 1).with_lineage(MailId::NONE, MailId::NONE, None));

        slot.drain_available();

        // The send flushed through the pool `WakeSink`; the demux delivers it
        // to the seize-handle-less peer via `route_mail` on a worker thread.
        let poke = poke_rx.recv_timeout(Duration::from_secs(2)).expect("the pumped handler's peer send arrived");
        assert_eq!(poke.kind, Poke::ID, "the peer received the Poke kind");
        let decoded = Poke::decode_from_bytes(poke.payload.bytes()).expect("the Poke decodes");
        assert_eq!(decoded, Poke { note: 7 }, "the peer received the Poke value");
    }

    /// ADR-0164 §3: host ingress runs synchronously on the pump-owning
    /// thread under this actor's Local stamp, then drops its inbound-less
    /// context before returning. Each turn's buffered peer send therefore
    /// appears only after the closure exits, starts an independent root, and
    /// records `Sent` in the pumped actor's trace ring rather than the
    /// chassis-host ring.
    #[test]
    fn host_turn_runs_stamped_on_caller_and_flushes_fresh_peer_roots() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0010);
        let peer_id = mailbox_id_from_name(Peer::NAMESPACE);
        let (poke_tx, poke_rx) = mpsc::channel::<(Envelope, thread::ThreadId)>();
        fx.registry
            .try_register_inbox_with_id(
                peer_id,
                "test.pumped.peer",
                Arc::new(move |d: Envelope| {
                    d.discharge();
                    let _ = poke_tx.send((d, thread::current().id()));
                }) as Arc<dyn InboxHandler>,
            )
            .expect("register the peer inbox");

        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, None);
        let caller_thread = thread::current().id();
        let state_after_turn = slot.host_turn(|state, ctx| {
            assert_eq!(thread::current().id(), caller_thread, "host ingress stays on its caller thread");
            assert!(ActorTraceRing::try_with(|_| ()).is_some(), "the pumped actor's Local slots are stamped");
            state.pings = 41;
            ctx.actor::<Peer>().send(&Poke { note: 1 });
            assert!(
                matches!(poke_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "buffered peer work cannot run before the host closure returns",
            );
            state.pings
        });
        assert_eq!(state_after_turn, Some(41), "the closure result and state mutation are preserved");

        let (first, first_thread) =
            poke_rx.recv_timeout(Duration::from_secs(2)).expect("the first host-turn peer send arrived");
        assert_ne!(first_thread, caller_thread, "peer delivery runs on a pool worker, not inside the host turn");
        assert_eq!(first.root, first.mail_id, "an inbound-less host turn mints a fresh root");
        assert!(first.parent_mail.is_none(), "a host-originated root has no parent mail");
        assert_eq!(Poke::decode_from_bytes(first.payload.bytes()).expect("first Poke decodes"), Poke { note: 1 });

        slot.host_turn(|_, ctx| ctx.actor::<Peer>().send(&Poke { note: 2 })).expect("the slot remains live");
        let (second, second_thread) =
            poke_rx.recv_timeout(Duration::from_secs(2)).expect("the second host-turn peer send arrived");
        assert_ne!(second_thread, caller_thread, "the second peer delivery also runs off the host thread");
        assert_eq!(second.root, second.mail_id, "each host turn starts its own root");
        assert_ne!(second.root, first.root, "separate host turns never inherit one another's roots");
        assert_eq!(Poke::decode_from_bytes(second.payload.bytes()).expect("second Poke decodes"), Poke { note: 2 });

        let trace = slot
            .host_turn(|_, _| ActorTraceRing::try_with(ActorTraceRing::snapshot).expect("trace ring is stamped"))
            .expect("the slot remains live");
        let sent: Vec<(MailId, MailId)> = trace
            .iter()
            .filter_map(|entry| match &entry.event {
                TraceEvent::Sent { mail_id, root, parent_mail, sender, recipient, kind, .. }
                    if *sender == self_id && *recipient == peer_id && *kind == Poke::ID =>
                {
                    assert!(parent_mail.is_none(), "host-turn Sent traces have no parent");
                    Some((*mail_id, *root))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            sent,
            vec![(first.mail_id, first.root), (second.mail_id, second.root)],
            "both host-originated sends land in the pumped actor's trace ring",
        );
    }

    /// ADR-0164 §3: host ingress never hides a recursive pump. Self-mail is
    /// flushed to the slot's inbox when the context drops, but state does not
    /// observe it until the owner explicitly drains. A spent slot rejects the
    /// turn without invoking its closure.
    #[test]
    fn host_turn_queues_self_mail_until_drain_and_stops_after_shutdown() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0011);
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, Some(wake_tx));

        let state_after_turn = slot.host_turn(|state, ctx| {
            state.pings = 10;
            ctx.send_detached_to(self_id, &Ping { seq: 1 });
            state.pings
        });
        assert_eq!(state_after_turn, Some(10));
        assert_eq!(slot.read_state(|state| state.pings), Some(10), "self-mail did not dispatch re-entrantly");

        assert!(
            matches!(wake_rx.recv_timeout(Duration::from_secs(2)), Ok(PumpWake::Mail)),
            "the flushed self-mail reached the pumped inbox",
        );
        slot.drain_available();
        assert_eq!(slot.read_state(|state| state.pings), Some(11), "the explicit drain dispatches queued self-mail");

        slot.shutdown();
        let mut invoked = false;
        assert!(slot.host_turn(|_, _| invoked = true).is_none(), "a spent slot rejects host ingress");
        assert!(!invoked, "the rejected host-turn closure was not invoked");
    }

    /// ADR-0161 §Decision 2: `await_settlement_pumped` returns `Settled` when
    /// the settlement callback fires, and the mid-wait `Mail` wakes drain
    /// every queued envelope before it returns. A producer keeps mail arriving
    /// on the pumped mailbox while the driver blocks in the wait; the settling
    /// Ping (the awaited chain's only inflight) is pushed last, so when the
    /// pump dispatches it every earlier envelope is already dispatched.
    #[test]
    fn await_settlement_pumped_drains_every_queued_envelope_before_returning() {
        const IDLE_PINGS: u32 = 8;

        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0007);
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<PumpWake>();
        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, Some(wake_tx.clone()));

        // The awaited chain: one inflight, a Ping to the pumped mailbox whose
        // dispatch settles `root` and fires the decision-2 callback.
        let root = MailId::new(self_id, 1);
        fx.mailer.record_sent_inflight(root);
        let settled_tx = wake_tx;
        fx.settlement.subscribe_settlement_with(root, move || {
            let _ = settled_tx.send(PumpWake::Settled);
        });

        let mailer = Arc::clone(&fx.mailer);
        let producer = thread::spawn(move || {
            for seq in 0..IDLE_PINGS {
                // Disarmed (NONE lineage): arrival keeps the pump busy but
                // carries no settlement obligation.
                let bytes = Ping { seq }.encode_into_bytes();
                mailer.push(Mail::new(self_id, Ping::ID, bytes, 1).with_lineage(MailId::NONE, MailId::NONE, None));
                thread::sleep(Duration::from_millis(1));
            }
            // The settling Ping, pushed last, with real lineage on `root`.
            let mail_id = MailId::new(self_id, 2);
            let bytes = Ping { seq: IDLE_PINGS }.encode_into_bytes();
            mailer.push(Mail::new(self_id, Ping::ID, bytes, 1).with_lineage(mail_id, root, None));
        });

        let outcome = await_settlement_pumped(
            &wake_rx,
            &mut slot,
            "test.drain.pumped",
            Duration::from_millis(50),
            Duration::from_secs(10),
            TerminalDisposition::Panic,
        );
        producer.join().expect("producer thread joins");

        assert!(matches!(outcome, WaitOutcome::Settled), "the wait returned Settled when the callback fired");
        assert_eq!(
            slot.read_state(|s| s.pings),
            Some(IDLE_PINGS + 1),
            "every queued envelope — the idle pings and the settling ping — dispatched before the wait returned",
        );
    }

    /// ADR-0161 §Context (the deadlock shape): the awaited chain's only
    /// inflight is a mail addressed to the pumped mailbox itself, so the root
    /// settles only when that mail is dispatched — and it is dispatched only
    /// because the wait pumps the slot on the `Mail` wake. The round budget is
    /// far larger than the test's runtime, so nothing but the wake can drive
    /// the drain; a broken wake would wedge and the `Panic` disposition would
    /// fail the test at the gate.
    #[test]
    fn await_settlement_pumped_settles_deadlock_shape_only_by_pumping() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0008);
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<PumpWake>();
        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, Some(wake_tx.clone()));

        let root = MailId::new(self_id, 1);
        let mail_id = MailId::new(self_id, 2);
        fx.mailer.record_sent_inflight(root);
        let settled_tx = wake_tx;
        fx.settlement.subscribe_settlement_with(root, move || {
            let _ = settled_tx.send(PumpWake::Settled);
        });
        // The mail on the awaited chain, addressed to the pumped mailbox.
        // Pushing it fires the slot's `Mail` wake — the only thing that can
        // drain it while the driver blocks below.
        let bytes = Ping { seq: 3 }.encode_into_bytes();
        fx.mailer.push(Mail::new(self_id, Ping::ID, bytes, 1).with_lineage(mail_id, root, None));

        let outcome = await_settlement_pumped(
            &wake_rx,
            &mut slot,
            "test.deadlock.pumped",
            Duration::from_millis(50),
            Duration::from_secs(2),
            TerminalDisposition::Panic,
        );

        assert!(matches!(outcome, WaitOutcome::Settled), "the pump settled the chain gated on its own mailbox");
        assert_eq!(slot.read_state(|s| s.pings), Some(1), "the awaited mail was dispatched by the pump, not the timer");
    }

    /// ADR-0161 §Decision 2: cap exhaustion wedges with the gate name
    /// attributable — the pumped mirror of
    /// `await_internal_signal_cap_exhaustion_wedges`. The pumped mailbox stays
    /// empty and no wake ever sends, so the wait exhausts its cumulative cap
    /// silently (the sender is held alive, so this is the silent path, not a
    /// disconnect).
    #[test]
    fn await_settlement_pumped_cap_exhaustion_wedges_attributable() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_0009);
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<PumpWake>();
        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, Some(wake_tx.clone()));

        let outcome = await_settlement_pumped(
            &wake_rx,
            &mut slot,
            "test.cap.pumped",
            Duration::from_millis(5),
            Duration::from_millis(20),
            TerminalDisposition::ReplyErr,
        );
        match outcome {
            WaitOutcome::Wedged(w) => {
                assert!(!w.disconnected, "cap exhaustion is the silent path, not a disconnect");
                assert_eq!(w.gate, "test.cap.pumped", "the wedge names the gate attributably");
                assert!(w.waited >= Duration::from_millis(20), "the wedge waited out the cumulative cap");
            }
            WaitOutcome::Settled => panic!("expected a wedge, got Settled"),
        }
        // Held to here so the channel stays connected — the wedge above is the
        // silent-to-cap path, distinct from `Disconnected`.
        drop(wake_tx);
    }

    /// ADR-0161 §Decision 2: a disconnected wake channel takes the same
    /// terminal path as cap exhaustion with `disconnected` set — the pumped
    /// mirror of `await_internal_signal_disconnect_wedges`. No wake sender is
    /// installed on the slot, so dropping the last `Sender` disconnects.
    #[test]
    fn await_settlement_pumped_disconnect_wedges_attributable() {
        let fx = fixtures();
        let self_id = MailboxId(0x_0DED_000A);
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<PumpWake>();
        let mut slot = boot_probe(&fx, self_id, PumpProbe::default(), false, None);
        drop(wake_tx);

        let outcome = await_settlement_pumped(
            &wake_rx,
            &mut slot,
            "test.disconnect.pumped",
            Duration::from_millis(50),
            Duration::from_secs(5),
            TerminalDisposition::Proceed,
        );
        match outcome {
            WaitOutcome::Wedged(w) => {
                assert!(w.disconnected, "dropping every sender disconnects the wake channel");
                assert_eq!(w.gate, "test.disconnect.pumped");
            }
            WaitOutcome::Settled => panic!("expected a wedge, got Settled"),
        }
    }
}
