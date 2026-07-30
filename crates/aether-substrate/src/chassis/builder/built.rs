use std::any::{Any, TypeId};
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;

use aether_actor::Root;

use super::boot_passives::BootedPassives;
use super::driver::{DriverRunning, RunError, assemble_pumped_slot};
use crate::actor::native::NativeActor;
use crate::actor::native::pumped_slot::PumpedSlot;
use crate::chassis::Chassis;
use crate::chassis::ctx::{MailboxWakeSlot, RelayInbox, prepare_relay_inbox};
use crate::chassis::error::BootError;
use crate::chassis::inbox::SettlingInbox;
use crate::chassis::settlement::SettlementRegistry;
use crate::mail::MailboxId;
use crate::mail::registry::effect::RegistryEffectError;

macro_rules! chassis_accessors {
    () => {
        #[must_use]
        pub fn resolve_actor<A: aether_actor::Instanced + NativeActor>(&self, subname: &str) -> Option<MailboxId> {
            resolve_actor::<A>(&self.booted, subname)
        }

        #[must_use]
        pub fn resolve_actors<A: aether_actor::Instanced + NativeActor>(&self) -> Vec<(String, MailboxId)> {
            resolve_actors::<A>(&self.booted)
        }

        pub fn spawn_actor<'a, A>(
            &'a self,
            subname: crate::Subname<'a>,
            config: A::Config,
            params: A::Params,
        ) -> crate::SpawnBuilder<'a, A>
        where
            A: Root + aether_actor::Instanced + NativeActor,
        {
            spawn_actor(&self.booted, subname, config, params)
        }

        #[must_use]
        pub fn actor_registry(&self) -> &Arc<crate::ActorRegistry> {
            actor_registry(&self.booted)
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
}

/// A chassis built without a driver. The embedder (`SubstrateHarness`, future
/// embedded harnesses) drives any loop manually. Passives are booted
/// and addressable via [`Self::resolve_actor`] / [`Self::resolve_actors`];
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
            match assemble_pumped_slot::<A>(mailbox_id, inbox, spawner, config, params) {
                Ok(slot) => registry
                    .promote_starting_through_owner(mailbox_id, token, handler)
                    .map(|()| (slot, wake_slot))
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
    /// produces — a flat `{NAMESPACE}:{subname}` id — so a test reaches the
    /// spawned actor through the ordinary [`Self::resolve_actor`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_actor_for_test<'a, A>(
        &'a self,
        subname: crate::Subname<'a>,
        config: A::Config,
        params: A::Params,
    ) -> crate::SpawnBuilder<'a, A>
    where
        A: aether_actor::Instanced + NativeActor,
    {
        spawn_actor(&self.booted, subname, config, params)
    }

    chassis_accessors!();
}

/// Surface an owner refusal as the chassis boot error the pumped boot path
/// already returns for every other failure.
fn owner_boot_error(error: &RegistryEffectError) -> BootError {
    BootError::Other(Box::new(io::Error::other(format!("registry owner refused the pumped activation: {error}"))))
}

fn resolve_actor<A: aether_actor::Instanced + NativeActor>(
    booted: &BootedPassives,
    subname: &str,
) -> Option<MailboxId> {
    // ADR-0099 §3: a nested actor's id is its lineage fold, not
    // `hash(NAMESPACE:subname)`, so resolve by the *registered* id —
    // walk the live instances of `A` and match the subname — rather
    // than recomputing a flat name-hash that only lands for a depth-1
    // (chassis-level) instance.
    resolve_actors::<A>(booted).into_iter().find(|(sn, _)| sn == subname).map(|(_, id)| id)
}

fn resolve_actors<A: aether_actor::Instanced + NativeActor>(booted: &BootedPassives) -> Vec<(String, MailboxId)> {
    booted.actor_registry.live_subnames_of_type::<A>()
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
    A: aether_actor::Instanced + NativeActor,
{
    crate::SpawnBuilder::new(
        Arc::clone(&booted.spawner),
        subname,
        config,
        params,
        crate::Source::NONE,
        // Chassis-level spawn: a top-level instanced actor with no
        // parent actor, so it is the depth-1 root of its own lineage
        // (ADR-0099 §3) and keeps the flat `{NAMESPACE}:{subname}` id.
        None,
    )
}

fn actor_registry(booted: &BootedPassives) -> &Arc<crate::ActorRegistry> {
    &booted.actor_registry
}

fn handle<H: Any + Send + Sync + Clone + 'static>(booted: &BootedPassives) -> Option<H> {
    booted.handles.get::<H>()
}
