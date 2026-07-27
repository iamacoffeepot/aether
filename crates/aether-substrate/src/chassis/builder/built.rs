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
use crate::chassis::ctx::{MailboxWakeSlot, register_relay_inbox};
use crate::chassis::error::BootError;
use crate::chassis::inbox::SettlingInbox;
use crate::chassis::settlement::SettlementRegistry;
use crate::mail::MailboxId;

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
    /// Claim-stage driver mailbox), registers a relay inbox under it, and
    /// runs the shared `assemble_pumped_slot` boot (binding + inbox
    /// install + seed + `init` / `wire`), returning the slot plus its
    /// [`MailboxWakeSlot`] so the embedder installs whatever wake nudges its
    /// pump cadence (or none — the harness busy-polls its drain).
    ///
    /// Errors if `A::NAMESPACE` is already owned by a different actor type,
    /// if the relay-inbox registration fails, or if `A::init` returns `Err`;
    /// in each failure the namespace claim is released before returning.
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
        let boot = register_relay_inbox(mailer.registry(), A::NAMESPACE).and_then(|(mailbox_id, rx, wake_slot)| {
            let inbox = SettlingInbox::new(mailbox_id, rx, Arc::clone(mailer));
            let slot = assemble_pumped_slot::<A>(mailbox_id, inbox, spawner, config, params)?;
            Ok((slot, wake_slot))
        });
        match boot {
            Ok(pair) => Ok(pair),
            Err(e) => {
                actor_registry.release_namespace(A::NAMESPACE, TypeId::of::<A>());
                Err(e)
            }
        }
    }

    chassis_accessors!();
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

fn spawn_actor<'a, A>(
    booted: &'a BootedPassives,
    subname: crate::Subname<'a>,
    config: A::Config,
    params: A::Params,
) -> crate::SpawnBuilder<'a, A>
where
    A: Root + aether_actor::Instanced + NativeActor,
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
