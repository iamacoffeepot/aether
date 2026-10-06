use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aether_actor::{ActorRef, Addressable, CastTarget, ChildOf, ErasedActorRef, Instanced, ProtocolRef, Root};
use aether_data::{ErasedActorPath, Kind, KindId, LoadName, MailId, ReplyContract, SessionToken};
use aether_kinds::{CostTail, CostTailResult};
use crossbeam_channel::Receiver;

use super::boot_passives::BootedPassives;
use super::driver::{DriverRunning, RunError, assemble_pumped_slot};
use super::root_pusher::RootPusher;
use super::route_probe::RouteReadProbe;
use super::target::ChassisTarget;
use crate::actor::native::NativeActor;
use crate::actor::native::slot::pumped::PumpedSlot;
use crate::chassis::Chassis;
use crate::chassis::ctx::{MailboxClaim, MailboxWakeSlot};
use crate::chassis::error::BootError;
use crate::chassis::inbox::{SettlingInbox, inbox_channel};
use crate::chassis::settlement::SettlementRegistry;
use crate::mail::boundary::{self, BoundaryMail};
use crate::mail::registry::effect::RegistryEffectError;
use crate::mail::registry::{
    AddressResolutionError, AdoptRefused, ChildRefused, Registry, ResolvedAddress, RouteContract,
};
use crate::runtime::effect_chain::Uncaused;
#[cfg(any(test, feature = "test-support"))]
use crate::testing::await_settled;

macro_rules! chassis_accessors {
    () => {
        /// Resolve a canonical or ADR-0166 short
        /// [`ErasedActorPath`](aether_data::ErasedActorPath) to the position of one live
        /// mailbox. This is the host's boundary parser (ADR-0230 §3), the one
        /// place an address becomes a position; it answers with a position
        /// and no proof, for an embedder that holds no spawn result for the
        /// actor it observes.
        ///
        /// # Errors
        ///
        /// Returns the parser's [`AddressResolutionError`] when the address
        /// names an unknown root, expands ambiguously, or names no live
        /// mailbox.
        pub fn resolve_address(
            &self,
            address: &aether_data::ErasedActorPath,
        ) -> Result<ResolvedAddress, AddressResolutionError> {
            self.booted.spawner.resolve_address(address)
        }

        /// Block until the root the pooled instanced actor at `address` ran
        /// its `wire` under has settled (ADR-0244 §7): every mail its `wire`
        /// sent, and everything those mails caused, has been handled. The
        /// **test-scoped** wait on one birth's `wire`, gated on the
        /// `test-support` feature like `await_closed` and
        /// `await_boot_settled`. It waits on settlement, never on the clock.
        ///
        /// It covers every birth that opened a wire root of its own: a guest a
        /// handler loaded, a child a handler staged, and an embedder spawn.
        /// The address is resolved through the boundary parser, as
        /// [`Self::resolve_address`] does, so a test that holds only the path
        /// of an actor it did not spawn itself, such as an autoloaded guest,
        /// can wait on it. The first call takes the settlement, and every
        /// later call on the same actor returns at once. The wait covers an
        /// actor that is still open: an actor's subscription to its wire
        /// root is released with its slot when it closes.
        ///
        /// # Panics
        /// Panics naming the `chassis.wire_settled` gate when `address`
        /// resolves to no live mailbox, when it names no pooled instanced
        /// actor, when the actor closed before the wait, when the actor's
        /// `wire` ran under the boot's root (await `await_boot_settled` for
        /// it), or when the root does not settle within the settlement cap
        /// (`AETHER_SETTLEMENT_CAP_SECS`).
        #[cfg(any(test, feature = "test-support"))]
        pub fn await_wire_settled(&self, address: &aether_data::ErasedActorPath) {
            self.booted.spawner.await_wire_settled(
                self.booted
                    .spawner
                    .resolve_address(address)
                    .unwrap_or_else(|error| panic!("chassis.wire_settled: {address} names no live actor: {error}"))
                    .mailbox_id,
                "chassis.wire_settled",
            );
        }

        pub fn spawn_actor<'a, A>(
            &'a self,
            subname: crate::Subname<'a>,
            config: A::Config,
            params: A::Params,
        ) -> crate::SpawnBuilder<'a, A>
        where
            A: Root + Instanced + NativeActor,
        {
            spawn_actor(&self.booted, subname, config, params)
        }

        /// The proven reference of the root actor `R` this chassis composed —
        /// a singleton capability or a pumped actor (ADR-0230 section 3).
        ///
        /// The boot that published `R`'s `Live` route recorded it; this reads
        /// the record by type and mints nothing. An instanced actor is never
        /// recorded, since its type can have many instances: its proof is the
        /// one its spawn's `finish` returned.
        ///
        /// # Panics
        ///
        /// Panics naming `R::NAMESPACE` when this chassis composed no `R`.
        /// Composition is a static fact of the chassis build, so asking for an
        /// uncomposed actor is a wiring bug in the caller, not a runtime state.
        #[must_use]
        pub fn actor_ref<R: Root + 'static>(&self) -> ActorRef<R> {
            actor_ref::<R>(&self.booted)
        }

        #[must_use]
        pub fn handle<H: Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
            handle::<H>(&self.booted)
        }
    };
}

/// A chassis built with a driver. [`Self::run`] delegates to the
/// driver's [`DriverRunning::run`] on the calling thread; when that
/// returns, every passive is shut down in reverse boot order, and then the
/// pumped roots the driver drove are closed.
pub struct BuiltChassis<C: Chassis> {
    pub(super) booted: BootedPassives,
    pub(super) driver: Box<dyn DriverRunning>,
    pub(super) _chassis: PhantomData<fn() -> C>,
}

impl<C: Chassis> fmt::Debug for BuiltChassis<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltChassis")
            .field("profile", &C::PROFILE)
            .field("passives", &self.booted.shutdowns.len())
            .finish_non_exhaustive()
    }
}

impl<C: Chassis> BuiltChassis<C> {
    chassis_accessors!();

    /// Block on the driver's run loop, then tear the chassis down in the
    /// one order it owns (ADR-0160 §3): the instanced actors, the composed
    /// roots in reverse boot order, and last the pumped roots the driver
    /// handed back. The pumped roots are dependencies of the others, so mail
    /// a closing actor leaves on one is dispatched by that root's own close.
    /// Driver errors propagate as [`RunError`]; the teardown runs before the
    /// error returns to the caller.
    pub fn run(self) -> Result<(), RunError> {
        let Self { booted, driver, .. } = self;
        let (result, pumped) = driver.run();

        // `BootedPassives::Drop` closes the instanced actors and then the
        // composed roots. Each pumped slot closes in its own drop, on this
        // thread, which is the one that pumped it.
        drop(booted);
        drop(pumped);

        result
    }

    /// Push typed `mail` to the actor `to` proves, untracked, with its reply
    /// routed to `reply` — the embedder's **test-scoped** push for a driven
    /// chassis that is built but never [`run`](Self::run).
    ///
    /// [`PassiveChassis::send_for_reply`] is the same push for a chassis with
    /// no driver. A production chassis drives itself and takes its mail over
    /// its own capabilities, so it gets no embedder send door: this method is
    /// gated on the `test-support` feature, and no production chassis can
    /// reach it. The bloomery harness is the motivating caller — it builds the
    /// shipped bloomery chassis, holds the references its mount took back, and
    /// drives the journal owner and the bundle driver in process.
    #[cfg(any(test, feature = "test-support"))]
    pub fn send_for_reply<K: Kind, I>(&self, to: impl ChassisTarget<K, I>, mail: &K, reply: ReplyTarget) {
        self.booted.spawner.push_for_reply(to.erased(), K::ID, mail.encode_into_bytes(), reply);
    }

    /// Push typed `mail` to the actor `to` proves as a chassis-root mail and
    /// return the minted root beside the receiver that fires once its whole
    /// causal chain settles (ADR-0080 §6) — the tracked sibling of
    /// [`Self::send_for_reply`], gated on the `test-support` feature the same
    /// way.
    ///
    /// [`PassiveChassis::send_tracked`] is the same push for a chassis with
    /// no driver. The bloomery harness's `call` is the consumer: it asserts
    /// that a `Call`'s chain settles once its outcome is out.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn send_tracked<K: Kind, I>(
        &self,
        to: impl ChassisTarget<K, I>,
        mail: &K,
        reply: Option<ReplyTarget>,
    ) -> (MailId, Receiver<()>) {
        self.booted.spawner.push_tracked(to.erased(), K::ID, mail.encode_into_bytes(), reply)
    }
}

/// A chassis built without a driver. The embedder (`SubstrateHarness`, future
/// embedded harnesses) drives any loop manually. Passives are booted
/// and addressable via [`Self::resolve_address`];
/// they shut down when the `PassiveChassis` is dropped.
pub struct PassiveChassis<C: Chassis> {
    pub(super) booted: BootedPassives,
    /// The pumped slots reserved at the Claim stage
    /// ([`Builder::reserve_pumped`](super::Builder::reserve_pumped)), keyed by
    /// namespace and moved off `booted` at build. An entry leaves the map
    /// when [`Self::boot_pumped_actor`] boots its actor. A recovery whose boot
    /// failed leaves `None` behind, so the slot still counts as never booted
    /// when the build's start closure returns.
    pub(super) reserved: Mutex<HashMap<String, Option<MailboxClaim>>>,
    pub(super) _chassis: PhantomData<fn() -> C>,
}

impl<C: Chassis> fmt::Debug for PassiveChassis<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PassiveChassis")
            .field("profile", &C::PROFILE)
            .field("passives", &self.booted.shutdowns.len())
            .finish_non_exhaustive()
    }
}

impl<C: Chassis> PassiveChassis<C> {
    /// The per-chassis actor lifecycle registry, for this crate's own
    /// chassis tests, which assert on its monitor, tombstone, and liveness
    /// bookkeeping. Test-only and crate-private: no embedder reads it.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn actor_registry(&self) -> &Arc<crate::ActorRegistry> {
        self.booted.spawner.actor_registry()
    }

    /// Number of booted passives. Useful for tests; not expected to
    /// vary at runtime.
    #[must_use]
    pub fn len(&self) -> usize {
        self.booted.shutdowns.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.booted.shutdowns.is_empty()
    }

    /// ADR-0080 §6: borrow the chassis-owned settlement registry.
    /// PR 4 lifecycle / frame / `replace_component` gate sites reach
    /// for this to call `subscribe_settlement(root)`; PR 3 surfaces
    /// the accessor for tests that pump synthetic events through the
    /// trace pipeline and wait on the resulting `Settled` signal.
    #[must_use]
    pub fn settlement_registry(&self) -> &Arc<SettlementRegistry> {
        self.booted.settlement_registry()
    }

    /// Whether `actor` would dispatch `kind`: a declared handler or a
    /// `#[fallback]` (ADR-0033), as the capability registry reflects after
    /// load / replace / drop.
    ///
    /// Consumer: `SubstrateHarness::accepts`.
    #[must_use]
    pub fn accepts(&self, actor: ErasedActorRef, kind: KindId) -> bool {
        self.booted.spawner.mailer().capability_registry().accepts_actor(actor, kind)
    }

    /// The contract the route `actor` proves publishes (ADR-0231 §4): its
    /// `(KindId, ReplyContract)` rows sorted by kind, and whether it has a
    /// `#[fallback]`. `None` while the route does not resolve `Live`.
    ///
    /// Consumer: `SubstrateHarness::published_contract`.
    #[must_use]
    pub fn published_contract(&self, actor: ErasedActorRef) -> Option<(Vec<(KindId, ReplyContract)>, bool)> {
        self.booted.spawner.mailer().registry().published_contract(actor.id()).map(RouteContract::into_parts)
    }

    /// Type an erased reference `actor` as the protocol `P` (ADR-0231 §4's
    /// guard cast), or answer `None`: the same registry cast
    /// [`NativeCtx::cast`](crate::actor::native::NativeCtx::cast) calls. It
    /// reads the route's `Live` published rows once, and `P::admits` applies
    /// the exact-rows rule.
    ///
    /// Consumer: `SubstrateHarness::cast`.
    #[must_use]
    pub fn cast<P: CastTarget>(&self, actor: ErasedActorRef) -> Option<ProtocolRef<P>> {
        self.booted.spawner.mailer().registry().cast(actor)
    }

    /// `actor`'s per-handler cost rows (ADR-0036), filtered by `request`: what
    /// the `actor_cost` MCP tool reports.
    ///
    /// Consumer: `SubstrateHarness::actor_cost`.
    #[must_use]
    pub fn actor_cost(&self, actor: ErasedActorRef, request: &CostTail) -> CostTailResult {
        self.booted.spawner.mailer().cost_table().tail(actor, request)
    }

    /// The id registered under the kind name `name`, or `None` when no kind of
    /// that name is registered.
    ///
    /// Consumer: `SubstrateHarness::count_observed`.
    #[must_use]
    pub fn kind_id(&self, name: &str) -> Option<KindId> {
        self.booted.spawner.registry().kind_id(name)
    }

    /// A diagnostic label for `kind`: its registered name, else its tagged id.
    ///
    /// Consumers: `SubstrateHarness::observed_kinds` and the harness's failure
    /// diagnostics.
    #[must_use]
    pub fn kind_label(&self, kind: KindId) -> String {
        self.booted.spawner.registry().kind_label(kind)
    }

    /// Every root the settlement table still counts as pending, as
    /// `(root, in_flight, held_open)` (ADR-0080 §6) — the dump a wedged
    /// settlement gate reports.
    ///
    /// Consumer: `SubstrateHarness`'s settlement-timeout diagnostic.
    #[must_use]
    pub fn pending_settlement_roots(&self) -> Vec<(MailId, u32, u32)> {
        self.booted.spawner.mailer().trace_handle().settlement_counter().pending_roots()
    }

    /// A read-only probe of the route table's hot read path.
    ///
    /// Consumer: the registry benchmark (`aether-harness-substrate`'s
    /// `perf::registry`, the `aether-perf-registry` binary).
    #[must_use]
    pub fn route_read_probe(&self) -> RouteReadProbe {
        RouteReadProbe::new(Arc::clone(self.booted.spawner.registry()))
    }

    /// ADR-0161 slice R4: boot a [`PumpedSlot`] for an externally-pumped
    /// actor `A` on this no-driver chassis, the passive counterpart of
    /// [`DriverCtx::boot_pumped_actor`](super::DriverCtx::boot_pumped_actor).
    /// The substrate harness is the embedder-as-driver: it owns the pumped
    /// render slot and drains it at its step / capture pump points, so it
    /// boots the slot here, inside the start closure of
    /// [`Builder::build_passive_with_start`](super::Builder::build_passive_with_start),
    /// rather than through a driver's Start-stage `boot`.
    ///
    /// Two cases, by whether the chassis reserved `A::NAMESPACE` at the Claim
    /// stage with [`Builder::reserve_pumped`](super::Builder::reserve_pumped):
    ///
    /// - **Reserved.** The reservation's inbox route has been live since
    ///   Claim, so a passive that declares a dependency on `A` passed its
    ///   birth check and any mail sent to `A` since waits in that inbox. The
    ///   boot recovers the reservation, assembles the slot from it and records
    ///   `A`'s reference. It writes nothing to the registry.
    /// - **Not reserved.** Claims `A::NAMESPACE` fresh and runs the two-ack
    ///   activation handshake against the ADR-0165 registry owner, below.
    ///
    /// Either way it returns the slot plus its [`MailboxWakeSlot`], and the
    /// embedder installs the wake its pump waits on:
    /// [`install_pump_wake`](crate::chassis::settlement::install_pump_wake)
    /// for a wait that only drains, as the test-support
    /// `testing::PumpedDriver` does, or a hook that also turns the embedder's
    /// own loop, as the desktop driver installs.
    /// A slot with no wake drains only when its owner calls
    /// [`PumpedSlot::drain_available`], so no wait on it can be woken by mail.
    ///
    /// The fresh claim runs post-seal by construction: the build seals
    /// immediately before handing out the `PassiveChassis` this is called on,
    /// so the route cannot be written directly and both acks go through the
    /// owner:
    ///
    /// 1. reserve `A::NAMESPACE` as a `Starting` route and take its token —
    ///    from here mail addressed to the actor parks in the owner instead of
    ///    warn-dropping against a name that does not exist yet;
    /// 2. run the shared `assemble_pumped_slot` boot — binding, inbox install,
    ///    seed, `init` and `wire` — **on this thread**, which is the pumped
    ///    actor's execution home, so actor-authored lifecycle code never runs
    ///    on the registry owner;
    /// 3. hand the owner the wired endpoint, which publishes the route `Live`
    ///    and releases everything parked behind step 1 in the order it arrived.
    ///
    /// Errors if the publication table refuses `A` its namespace (another
    /// type sharing it was born first), if the owner refuses either ack, if
    /// `A::init` returns `Err`, or if an earlier boot of the same reservation
    /// already failed; in each failure any accepted `Starting` reservation is
    /// cancelled before returning, and an actor that had already wired is
    /// closed. The namespace hold is never released: a
    /// failed boot fails the build (R-0046). A failed boot of a Claim-stage
    /// reservation leaves the slot unbooted, so the build fails too.
    pub fn boot_pumped_actor<A>(
        &self,
        config: A::Config,
        params: A::Params,
    ) -> Result<(PumpedSlot<A>, Arc<MailboxWakeSlot>), BootError>
    where
        A: Root + NativeActor,
    {
        let spawner = &self.booted.spawner;
        spawner.registry().hold_native::<A>().map_err(|refusal| BootError::Other(Box::new(refusal)))?;

        if let Some(recovered) = self.recover_reservation(A::NAMESPACE) {
            recovered.and_then(|MailboxClaim { id: mailbox_id, inbox, wake_slot, .. }| {
                let slot =
                    assemble_pumped_slot::<A>(mailbox_id, inbox, spawner, config, params, Uncaused::EmbedderCall)?;
                // ADR-0231 §4: the reservation went `Live` at the Claim stage,
                // before `A` was known, so its route publishes `A`'s contract
                // now, through the owner, and only then releases `wire`'s mail.
                if let Err(error) =
                    spawner.mailer().registry().publish_contract_through_owner(mailbox_id, RouteContract::of::<A>())
                {
                    // The actor wired, so it closes: the slot's drop runs
                    // the one close, which discards what `wire` sent.
                    drop(slot);
                    return Err(owner_boot_error(&error));
                }
                slot.release_outbound_after_activation();
                // ADR-0230: the Claim-stage reservation published the route
                // before the seal and the actor is now wired.
                self.booted.references.record(Registry::activated::<A>(mailbox_id));
                self.lock_reserved().remove(A::NAMESPACE);
                Ok((slot, wake_slot))
            })
        } else {
            let mailer = spawner.mailer();
            let registry = mailer.registry();
            registry.reserve_starting_through_owner(A::NAMESPACE).map_err(|error| owner_boot_error(&error)).and_then(
                |(mailbox_id, token)| {
                    let (receiver, relay) = inbox_channel(mailer);
                    let wake_slot = Arc::clone(relay.wake_slot());
                    let inbox = SettlingInbox::new_at(mailbox_id, receiver, Arc::clone(mailer));
                    match assemble_pumped_slot::<A>(mailbox_id, inbox, spawner, config, params, Uncaused::EmbedderCall)
                    {
                        Ok(slot) => match registry.promote_starting_through_owner(
                            mailbox_id,
                            token,
                            relay,
                            RouteContract::of::<A>(),
                        ) {
                            // ADR-0230: the owner has published the route
                            // `Live` with its contract, so `wire`'s mail goes.
                            Ok(()) => {
                                slot.release_outbound_after_activation();
                                self.booted.references.record(Registry::activated::<A>(mailbox_id));
                                Ok((slot, wake_slot))
                            }
                            // The actor wired, so it closes: the slot's
                            // drop runs the one close, which discards what
                            // `wire` sent.
                            Err(error) => {
                                drop(slot);
                                Err(owner_boot_error(&error))
                            }
                        },
                        Err(e) => {
                            registry.cancel_starting_through_owner(mailbox_id, token);
                            Err(e)
                        }
                    }
                },
            )
        }
    }

    /// Take the Claim-stage reservation under `namespace`. `None` when the
    /// chassis reserved nothing there. `Some(Err)` when the reservation was
    /// already recovered by a boot that failed, which leaves the slot
    /// unbooted for good.
    fn recover_reservation(&self, namespace: &str) -> Option<Result<MailboxClaim, BootError>> {
        let claim = self.lock_reserved().get_mut(namespace).map(Option::take)?;
        Some(claim.ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(format!(
                "pumped slot {namespace:?} was reserved at the Claim stage and its boot already failed"
            ))))
        }))
    }

    fn lock_reserved(&self) -> MutexGuard<'_, HashMap<String, Option<MailboxClaim>>> {
        self.reserved.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fail when a pumped slot reserved at the Claim stage was never booted:
    /// the passive build's completion check, run after its start closure.
    pub(super) fn check_reservations_booted(&self) -> Result<(), BootError> {
        check_reservations_booted(self.lock_reserved().keys().map(String::as_str))
    }

    /// Push typed `mail` to the actor `to` proves as a chassis-root mail and
    /// return the minted root beside the receiver that fires once its whole
    /// causal chain settles (ADR-0080 §6).
    ///
    /// The embedder's tracked send: the push is recorded as a root, so the
    /// trace pipeline follows every descendant mail. The root is minted from
    /// the engine's one chassis-root counter, never chosen by the caller;
    /// `reply`, when present, routes the recipient's reply to a hub session
    /// or another proven actor. The embedder holds a proof, never a position.
    #[must_use]
    pub fn send_tracked<K: Kind, I>(
        &self,
        to: impl ChassisTarget<K, I>,
        mail: &K,
        reply: Option<ReplyTarget>,
    ) -> (MailId, Receiver<()>) {
        self.booted.spawner.push_tracked(to.erased(), K::ID, mail.encode_into_bytes(), reply)
    }

    /// Prove one boundary-named recipient path and bind it to the supplied
    /// kind and payload bytes. The returned item can only be delivered.
    pub fn accept_call(
        &self,
        recipient: &ErasedActorPath,
        kind: KindId,
        payload: Vec<u8>,
    ) -> Result<BoundaryMail, String> {
        boundary::accept_call(self.booted.spawner.registry(), recipient, kind, payload)
    }

    /// Deliver a path-proven boundary item as a tracked chassis root.
    #[must_use]
    pub fn deliver_tracked(&self, item: BoundaryMail, reply: Option<ReplyTarget>) -> (MailId, Receiver<()>) {
        let BoundaryMail { recipient, kind, payload } = item;
        self.booted.spawner.push_tracked(recipient, kind, payload, reply)
    }

    /// Deliver a path-proven boundary item with its reply routed to `reply`.
    pub fn deliver_for_reply(&self, item: BoundaryMail, reply: ReplyTarget) {
        let BoundaryMail { recipient, kind, payload } = item;
        self.booted.spawner.push_for_reply(recipient, kind, payload, reply);
    }

    /// Name the canonical path retained for an erased reference.
    #[must_use]
    pub fn actor_path(&self, reference: ErasedActorRef) -> Option<ErasedActorPath> {
        self.booted.spawner.registry().actor_path(reference)
    }

    /// The chassis-root door to the composed root actor `R` (ADR-0080 §6),
    /// for a no-driver chassis's embedder — the passive counterpart of
    /// [`DriverCtx::root_pusher`](super::DriverCtx::root_pusher).
    ///
    /// # Panics
    ///
    /// Panics naming `R::NAMESPACE` when this chassis composed no `R`, like
    /// [`Self::actor_ref`].
    #[must_use]
    pub fn root_pusher<R: Root + 'static>(&self) -> RootPusher<R> {
        self.booted.spawner.root_pusher(actor_ref::<R>(&self.booted))
    }

    /// Push typed `mail` to the actor `to` proves, untracked, with its reply
    /// routed to `reply` — a hub session or another proven actor.
    pub fn send_for_reply<K: Kind, I>(&self, to: impl ChassisTarget<K, I>, mail: &K, reply: ReplyTarget) {
        self.booted.spawner.push_for_reply(to.erased(), K::ID, mail.encode_into_bytes(), reply);
    }

    /// Type the stamped sender of a successful load reply as the loaded actor
    /// `R` (ADR-0230 §3). A load reply is sent by the loaded actor itself, so
    /// the embedder reads its erased reference off the reply event; this
    /// narrows it once the registry confirms the sender is a live component
    /// trampoline. `R` is the export the embedder named in its load.
    ///
    /// Its consumer is the substrate harness's `load::<R>`.
    ///
    /// # Errors
    ///
    /// Returns [`AdoptRefused`] when the sender is no longer live or is not
    /// a loaded component.
    pub fn adopt_load<R: Addressable>(&self, sender: ErasedActorRef) -> Result<ActorRef<R>, AdoptRefused> {
        self.booted.spawner.adopt_loaded::<R>(sender)
    }

    /// The proven reference of the `Child` instance keyed by `key` directly
    /// beneath `parent` (ADR-0230 §3's child-beneath-a-held-reference door,
    /// for an embedder).
    ///
    /// The embedder holds the parent's proof — a composed capability's, a
    /// load's, or another child's — and names the child by type and key, so
    /// a child a component spawned, or a window a window capability opened,
    /// is reached without rendering or parsing an address. The key is folded
    /// beneath the parent's position and proven against the published routes;
    /// only a `Live` child answers.
    ///
    /// Its consumer is the substrate harness's `child::<P, C>`.
    ///
    /// # Errors
    ///
    /// Returns [`ChildRefused`], naming the key and `Child::NAMESPACE`, when no
    /// `Live` child stands at that key: never spawned, still `Starting`, or
    /// already dropped.
    pub fn child<Parent, Child>(&self, parent: ActorRef<Parent>, key: LoadName) -> Result<ActorRef<Child>, ChildRefused>
    where
        Parent: Addressable,
        Child: ChildOf<Parent> + Instanced,
    {
        self.booted.spawner.live_child::<Child>(parent.erase(), key)
    }

    /// Place an instanced `A` at the chassis root **for a test**, without
    /// asking for the ADR-0166 [`Root`] permission [`Self::spawn_actor`]
    /// requires.
    ///
    /// A placement permission is a link-time global fact — `#[actor(root)]`
    /// emits a `RootEntry` every binary that links the crate collects — so an
    /// actor that only ever ships as somebody's child must not declare `root`
    /// to satisfy a unit test. This is the authority that lets the test
    /// compose it anyway: test-scoped (the method is gated on the
    /// `test-support` feature, so no production chassis can reach it) and
    /// asserted nowhere in the inventory. `aether.fleet.proxy` is the
    /// motivating caller — production spawns it through
    /// [`NativeCtx::spawn_child`](crate::NativeCtx::spawn_child) under
    /// `aether.fleet`, while its own unit tests drive it against a fake RPC
    /// server with no engines cap in the picture.
    ///
    /// The placement is the same parentless depth-1 one `spawn_actor`
    /// produces — a flat `{NAMESPACE}:{subname}` id — and the builder's
    /// `finish()` answers the spawned actor's proven reference.
    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_actor_for_test<'a, A>(
        &'a self,
        subname: crate::Subname<'a>,
        config: A::Config,
        params: A::Params,
    ) -> crate::SpawnBuilder<'a, A>
    where
        A: Instanced + NativeActor,
    {
        spawn_actor(&self.booted, subname, config, params)
    }

    /// Block until the pooled instanced `actor` has run its close cycle and
    /// the registry owner has applied its route drop, so
    /// [`Self::published_contract`] answers `None` once this returns — the
    /// **test-scoped** wait for an actor's close, gated on the
    /// `test-support` feature like [`Self::spawn_actor_for_test`].
    ///
    /// It waits on the slot's own close-done signal and then on an empty
    /// barrier batch through the FIFO registry owner, never on the clock. An
    /// actor that has already closed has released its slot, so the call
    /// waits on the barrier alone, which lands behind the route drop the
    /// close queued. One waiter per actor at a time: a second concurrent
    /// call on the same actor displaces the first.
    ///
    /// # Panics
    /// Panics when `actor` is neither a pooled instanced actor that is still
    /// open nor an actor that has closed (a singleton or a pumped slot that
    /// is still open has no close-done signal here), or when the close does
    /// not apply within the settlement cap (`AETHER_SETTLEMENT_CAP_SECS`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn await_closed(&self, actor: ErasedActorRef) {
        self.booted.spawner.await_closed(actor.id(), "testing.await_closed");
    }

    /// Block until the registry owner has applied and published every
    /// batch submitted before this call — the **test-scoped** barrier for
    /// an effect this harness cannot observe directly (an alias a guest's
    /// `wire` staged, a republish a replace staged), gated on the
    /// `test-support` feature like [`Self::await_closed`].
    ///
    /// The owner runs one FIFO queue and applies and publishes a whole
    /// drain before it completes any batch in it, so this proves only
    /// batches submitted *before* the call: the caller must already hold a
    /// real ordering signal (a load reply, a settled follow-up mail) that
    /// the effect it cares about was submitted first.
    ///
    /// # Panics
    /// Panics when the registry owner refuses the barrier batch or does not
    /// complete it within the settlement cap (`AETHER_SETTLEMENT_CAP_SECS`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn await_registry_applied(&self) {
        self.booted.spawner.await_registry_applied("testing.await_registry_applied");
    }

    /// Block until the boot's wire root has settled (ADR-0244): every mail a
    /// boot `wire` sent — the capability `wire` pass, a pre-seal spawn, a
    /// driver's pumped actor — and everything those mails caused, has been
    /// handled. The **test-scoped** wait on boot, gated on the `test-support`
    /// feature like [`Self::await_closed`]. It waits on settlement, never on
    /// the clock; a boot whose `wire` sends nothing returns at once.
    ///
    /// Idempotent: the first call consumes the settlement, and every later
    /// call returns at once.
    ///
    /// # Panics
    /// Panics when the root does not settle within the settlement cap
    /// (`AETHER_SETTLEMENT_CAP_SECS`), naming the `chassis.boot_settled`
    /// gate. Work a boot `wire` starts on its own chain must finish for the
    /// root to settle; long-lived work opens a detached chain.
    #[cfg(any(test, feature = "test-support"))]
    pub fn await_boot_settled(&self) {
        let mut settled = self.booted.boot_settled.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(receiver) = settled.as_ref() {
            await_settled(receiver, "chassis.boot_settled");
            *settled = None;
        }
    }

    chassis_accessors!();
}

/// Where an embedder push ([`PassiveChassis::send_tracked`] or
/// [`PassiveChassis::send_for_reply`], and the test-scoped
/// `BuiltChassis::send_for_reply`) routes the reply its recipient sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyTarget {
    /// A hub session, correlated by `correlation` — the shape a harness
    /// driving the chassis over a loopback outbound awaits.
    Session { session: SessionToken, correlation: u64 },
    /// Another proven actor, which receives the reply as mail correlated by
    /// `correlation`.
    Actor { to: ErasedActorRef, correlation: u64 },
}

/// Fail naming every pumped slot reserved at the Claim stage that was never
/// booted (ADR-0230 §3: once a boot completes, every declared dependency is
/// live). `names` are the reservations still outstanding once the Start
/// stage is over: a driver's unrecovered `claim_driver_mailbox` stash, or a
/// passive build's unbooted `reserve_pumped` slots.
pub(super) fn check_reservations_booted<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), BootError> {
    let mut unbooted: Vec<String> = names.map(|name| format!("{name:?}")).collect();
    if unbooted.is_empty() {
        return Ok(());
    }
    unbooted.sort_unstable();
    Err(BootError::Other(Box::new(io::Error::other(format!(
        "pumped slot {} was reserved at the Claim stage but never booted",
        unbooted.join(", ")
    )))))
}

/// Surface an owner refusal as the chassis boot error the pumped boot path
/// already returns for every other failure.
fn owner_boot_error(error: &RegistryEffectError) -> BootError {
    BootError::Other(Box::new(io::Error::other(format!("registry owner refused the pumped activation: {error}"))))
}

// The `Root` bound lives on the callers, not here: `spawn_actor` is the
// permission-checked chassis surface, `spawn_actor_for_test` the
// test-support one, and both reach the same parentless placement through
// this shared body.
fn spawn_actor<'a, A>(
    booted: &'a BootedPassives,
    subname: crate::Subname<'a>,
    config: A::Config,
    params: A::Params,
) -> crate::SpawnBuilder<'a, A>
where
    A: Instanced + NativeActor,
{
    // Chassis-level spawn: a top-level instanced actor with no parent actor,
    // so it is the depth-1 root of its own lineage (ADR-0099 §3) and keeps
    // the flat `{NAMESPACE}:{subname}` id. `SpawnBuilder::new` is the
    // parentless constructor; a birth under a parent goes through
    // `NativeCtx::spawn_child`.
    crate::SpawnBuilder::new(Arc::clone(&booted.spawner), subname, config, params, crate::Source::NONE)
}

fn actor_ref<R: Root + 'static>(booted: &BootedPassives) -> ActorRef<R> {
    booted.references.get::<R>().unwrap_or_else(|| {
        panic!("this chassis composed no {:?} actor; compose it before asking for its reference", R::NAMESPACE)
    })
}

fn handle<H: Any + Send + Sync + Clone + 'static>(booted: &BootedPassives) -> Option<H> {
    booted.handles.get::<H>()
}
