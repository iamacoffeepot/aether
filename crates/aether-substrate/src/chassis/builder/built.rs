use std::any::{Any, TypeId};
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;

use aether_actor::{ActorRef, Addressable, ChildOf, ErasedActorRef, Instanced, Root, child_address};
use aether_data::{KindId, LoadName, SessionToken};
use crossbeam_channel::Receiver;

use super::boot_passives::BootedPassives;
use super::driver::{DriverRunning, RunError, assemble_pumped_slot};
use crate::actor::native::NativeActor;
use crate::actor::native::slot::pumped::PumpedSlot;
use crate::chassis::Chassis;
use crate::chassis::ctx::{MailboxWakeSlot, RelayInbox, prepare_relay_inbox};
use crate::chassis::error::BootError;
use crate::chassis::inbox::SettlingInbox;
use crate::chassis::settlement::SettlementRegistry;
use crate::mail::registry::effect::RegistryEffectError;
use crate::mail::registry::{AddressResolutionError, AdoptRefused, ChildRefused, Registry, ResolvedAddress};
use crate::runtime::effect_chain::Uncaused;

macro_rules! chassis_accessors {
    () => {
        /// Resolve a canonical or ADR-0166 short
        /// [`ActorPath`](aether_data::ActorPath) to the position of one live
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
            address: &aether_data::ActorPath,
        ) -> Result<ResolvedAddress, AddressResolutionError> {
            self.booted.spawner.resolve_address(address)
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

        #[must_use]
        pub fn actor_registry(&self) -> &Arc<crate::ActorRegistry> {
            actor_registry(&self.booted)
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
/// returns, every passive is shut down in reverse boot order.
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

    /// Block on the driver's run loop. On clean return, shut down
    /// every passive in reverse boot order. Driver errors propagate
    /// as [`RunError`]; passives still tear down before the error
    /// returns to the caller.
    pub fn run(self) -> Result<(), RunError> {
        let Self { booted, driver, .. } = self;
        let result = driver.run();
        // Passives drop here, triggering reverse-order shutdown via
        // BootedPassives::Drop. Holding `booted` until after `result`
        // is bound keeps shutdown ordering deterministic.
        drop(booted);
        result
    }

    /// Push `payload` to the actor `to` proves, untracked, with its reply
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
    pub fn send_for_reply(&self, to: ErasedActorRef, kind: KindId, payload: Vec<u8>, reply: ReplyTarget) {
        self.booted.spawner.push_for_reply(to, kind, payload, reply);
    }
}

/// A chassis built without a driver. The embedder (`SubstrateHarness`, future
/// embedded harnesses) drives any loop manually. Passives are booted
/// and addressable via [`Self::resolve_address`];
/// they shut down when the `PassiveChassis` is dropped.
pub struct PassiveChassis<C: Chassis> {
    pub(super) booted: BootedPassives,
    pub(super) _chassis: PhantomData<fn() -> C>,
}

impl<C: Chassis> fmt::Debug for PassiveChassis<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PassiveChassis")
            .field("profile", &C::PROFILE)
            .field("passives", &self.booted.shutdowns.len())
            .finish()
    }
}

impl<C: Chassis> PassiveChassis<C> {
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

    /// ADR-0161 slice R4: boot a [`PumpedSlot`] for an externally-pumped
    /// actor `A` on this no-driver chassis, the passive counterpart of
    /// [`DriverCtx::boot_pumped_actor`](super::DriverCtx::boot_pumped_actor).
    /// The substrate harness is the embedder-as-driver: it owns the pumped
    /// render slot and drains it at its step / capture pump points, so it
    /// claims the slot here after `build_passive` rather than through a
    /// driver's Start-stage `boot`.
    ///
    /// Claims `A::NAMESPACE` fresh (a no-driver chassis reserved no
    /// Claim-stage driver mailbox), then runs the two-ack activation
    /// handshake against the ADR-0165 registry owner, returning the slot plus
    /// its [`MailboxWakeSlot`] so the embedder installs whatever wake nudges
    /// its pump cadence (or none — the harness busy-polls its drain).
    ///
    /// This runs post-seal by construction: `build_passive` seals immediately
    /// before handing back the `PassiveChassis` this is called on, so the
    /// route cannot be written directly and both acks go through the owner:
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
    /// Errors if `A::NAMESPACE` is already owned by a different actor type, if
    /// the owner refuses either ack, or if `A::init` returns `Err`; in each
    /// failure the namespace claim is released and any accepted reservation is
    /// cancelled before returning.
    pub fn boot_pumped_actor<A>(
        &self,
        config: A::Config,
        params: A::Params,
    ) -> Result<(PumpedSlot<A>, Arc<MailboxWakeSlot>), BootError>
    where
        A: Root + NativeActor,
    {
        let spawner = &self.booted.spawner;
        let actor_registry = spawner.actor_registry();
        if actor_registry.try_claim_namespace(A::NAMESPACE, TypeId::of::<A>()).is_err() {
            return Err(BootError::Other(Box::new(io::Error::other(format!(
                "namespace {:?} already owned by a different TypeId — fix the conflicting actor's NAMESPACE const",
                A::NAMESPACE
            )))));
        }

        let mailer = spawner.mailer();
        let registry = mailer.registry();
        let reserved = registry.reserve_starting_through_owner(A::NAMESPACE).map_err(|error| owner_boot_error(&error));
        let boot = reserved.and_then(|(mailbox_id, token)| {
            let RelayInbox { receiver, wake_slot, handler } = prepare_relay_inbox();
            let inbox = SettlingInbox::new(mailbox_id, receiver, Arc::clone(mailer));
            match assemble_pumped_slot::<A>(mailbox_id, inbox, spawner, config, params, Uncaused::EmbedderCall) {
                Ok(slot) => registry
                    .promote_starting_through_owner(mailbox_id, token, handler)
                    .map(|()| {
                        // ADR-0230: the owner has published the route `Live`.
                        self.booted.references.record(Registry::activated::<A>(mailbox_id));
                        (slot, wake_slot)
                    })
                    .map_err(|error| owner_boot_error(&error)),
                Err(e) => {
                    registry.cancel_starting_through_owner(mailbox_id, token);
                    Err(e)
                }
            }
        });
        match boot {
            Ok(pair) => Ok(pair),
            Err(e) => {
                actor_registry.release_namespace(A::NAMESPACE, TypeId::of::<A>());
                Err(e)
            }
        }
    }

    /// Push `payload` to the actor `to` proves as a chassis-root mail and
    /// return the receiver that fires once its whole causal chain settles
    /// (ADR-0080 §6).
    ///
    /// The embedder's tracked send: the push is recorded as a root, so the
    /// trace pipeline follows every descendant mail. `correlation` names the
    /// root; `reply`, when present, routes the recipient's reply to a hub
    /// session or another proven actor. The embedder holds a proof, never a
    /// position.
    #[must_use]
    pub fn send_tracked(
        &self,
        to: ErasedActorRef,
        kind: KindId,
        payload: Vec<u8>,
        correlation: u64,
        reply: Option<ReplyTarget>,
    ) -> Receiver<()> {
        self.booted.spawner.push_tracked(to, kind, payload, correlation, reply)
    }

    /// Push `payload` to the actor `to` proves, untracked, with its reply
    /// routed to `reply` — a hub session or another proven actor.
    pub fn send_for_reply(&self, to: ErasedActorRef, kind: KindId, payload: Vec<u8>, reply: ReplyTarget) {
        self.booted.spawner.push_for_reply(to, kind, payload, reply);
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
    /// beneath `parent` (ADR-0230 §3's `Address<R>` door, for an embedder).
    ///
    /// The embedder holds the parent's proof — a composed capability's, a
    /// load's, or another child's — and names the child by type and key, so
    /// a child a component spawned, or a window a window capability opened,
    /// is reached without rendering or parsing an address. The child address
    /// is folded beneath the parent and proven against the published routes;
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
        self.booted.spawner.live_child(&child_address::<Parent, Child>(parent, key))
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

fn actor_registry(booted: &BootedPassives) -> &Arc<crate::ActorRegistry> {
    &booted.actor_registry
}

fn handle<H: Any + Send + Sync + Clone + 'static>(booted: &BootedPassives) -> Option<H> {
    booted.handles.get::<H>()
}
