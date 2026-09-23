//! Retiring this actor, and watching the ones it depends on.
//!
//! Issue 607 Phase 4 (ADR-0079) self-shutdown and ADR-0063 fail-fast on one
//! side; the ADR-0079 §8 monitor surface on the other — register a watch,
//! declare the caller's own mailbox vacated, or vacate a single inline-child
//! alias folded onto it (ADR-0114).

use std::sync::Arc;

use aether_actor::{ErasedActorRef, ReplyMode};
use aether_data::MailboxId;

use crate::actor::monitor::{MonitorHandle, notify_alias_departures, notify_departure};
use crate::actor::registry::MonitorError;

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// Issue 607 Phase 4a (ADR-0079): self-shutdown signal. Sets a
    /// flag the actor's dispatcher polls after each handler returns;
    /// when set, the trampoline drains any remaining inbox mail
    /// synchronously, runs `NativeActor::unwire`, and exits the
    /// dispatch loop. After exit the actor's [`MailboxId`]
    /// transitions from `Live` to `Dead` in the chassis's
    /// [`ActorRegistry`](crate::ActorRegistry) and is added to `tombstones` —
    /// `spawn_child` rejects reuse of the retired full name with
    /// `SpawnError::SubnameRetired`.
    ///
    /// Idempotent — flipping the flag twice is the same as flipping
    /// it once. Singletons booted through `with_actor` rely on the
    /// chassis-shutdown channel-drop path instead of this flag, but
    /// can call `shutdown()` to opt in to flag-based exit.
    pub fn shutdown(&self) {
        self.binding.signal_shutdown();
    }

    /// ADR-0063 fail-fast: bring the substrate down with `reason`.
    /// Diverging — does not return. Used by handlers that observe a
    /// non-recoverable invariant violation (today: the wasm trampoline
    /// on a guest trap). Native impl forwards to
    /// [`NativeBinding::fatal_abort`](crate::actor::native::binding::NativeBinding::fatal_abort). See also the
    /// [`aether_actor::wasm::WasmCtx`] counterpart, which `panic!`s — the
    /// substrate's wasm runtime catches the trap and ADR-0063 escalates
    /// symmetrically.
    pub fn fatal_abort(&self, reason: String) -> ! {
        self.binding.fatal_abort(reason);
    }

    /// Issue 607 Phase 4b (ADR-0079): register the calling actor as a
    /// monitor of `target`. Returns a [`MonitorHandle`] whose `Drop`
    /// deregisters the entry, so a handler that wants to unwatch
    /// before the watcher itself dies just drops the handle.
    ///
    /// The substrate drains the target's monitor list and fires one
    /// [`aether_kinds::MonitorNotice`] per watcher when the target
    /// goes away — on close (before the slot transitions
    /// `Live` → `Dead`) or on vacate ([`Self::vacate`]: the occupant
    /// unloads while the slot stays live), whichever comes first
    /// (ADR-0079 §8, amended). The watcher receives that notice as
    /// ordinary mail whose envelope sender is the departed actor, so its
    /// handler reads `ctx.sender()` to get the same [`ErasedActorRef`] it
    /// monitored; either way the notice means state keyed by that
    /// reference is stale.
    ///
    /// Validation: `target` is the ADR-0230 proof that an actor reached
    /// `Live` in the routing [`Registry`](crate::Registry). The runtime check
    /// still runs, and still fails, because the proof and the check read
    /// different tables: the proof reads the published routes, while the
    /// monitor index registers against the actor-slot map of the
    /// [`ActorRegistry`](crate::ActorRegistry).
    ///
    /// - [`MonitorError::TargetNotFound`] for a proven target with no `Live`
    ///   actor slot: a chassis singleton not yet lifted into the slot map, and
    ///   an inline-child alias (ADR-0114 §2), which is served by its target
    ///   parent's slot and so has none of its own. The alias case is why the
    ///   routing-registry fallback below stays — refusing an alias would make
    ///   every row a cap keys on an inline child's stamped identity
    ///   (ADR-0114 §4) unreclaimable.
    /// - [`MonitorError::TargetTombstoned`] for a proven target that has since
    ///   closed: a reference claims the actor reached `Live`, never that it is
    ///   `Live` now (ADR-0230 §1).
    /// - [`MonitorError::Unsupported`] for a caller whose transport has no
    ///   spawner wired (a test binding such as `testing::unrouted_binding`),
    ///   so there is no monitor index at all — a property of the caller's
    ///   binding, not of the target. Handlers that monitor their registrants
    ///   treat it as "not monitorable" and stay drivable under test bindings.
    pub fn monitor(&self, target: ErasedActorRef) -> Result<MonitorHandle, MonitorError> {
        let target = target.id();
        let spawner = self.binding.spawner().ok_or(MonitorError::Unsupported)?;
        let registry = Arc::clone(spawner.actor_registry());
        let watcher = self.binding.self_mailbox();
        match registry.register_monitor(watcher, target) {
            // ADR-0114 §2: an inline child's alias has no actor slot, so the
            // slot-keyed check answers `TargetNotFound` for an address that
            // is live and mailable. Its liveness lives in the routing
            // registry; take that as the authority for an alias.
            Err(MonitorError::TargetNotFound) if self.binding.mailer().registry().is_live_alias(target) => {
                registry.register_alias_monitor(watcher, target);
            }
            other => other?,
        }
        Ok(MonitorHandle::new(registry, watcher, target))
    }

    /// ADR-0079 §8 (amended): declare the calling actor's mailbox
    /// vacated — its occupant is gone while the actor itself stays
    /// live and addressable. Drains the caller's own watcher list and
    /// fires one [`aether_kinds::MonitorNotice`] per watcher, exactly
    /// as the close path does, but without tombstoning: monitors
    /// registered after the vacate watch the mailbox's next occupant
    /// (or its eventual close).
    ///
    /// The one production caller is the wasm trampoline's
    /// `DropComponent` handler — a component drop is a wasm unload
    /// behind a still-addressable, refillable mailbox, which the
    /// close fan-out never reaches by design. Self-service only: an
    /// actor can declare its own mailbox vacated, never a peer's.
    ///
    /// The departing occupant is a whole cluster (ADR-0114 §2): the
    /// mailbox itself plus every inline-child alias folded onto it, each
    /// of which drains and fires under its own name, so a cap holding rows
    /// keyed on an inline child's stamped identity (ADR-0114 §4) can
    /// reclaim them.
    ///
    /// The notice mail is pushed root-shaped (no parent chain),
    /// mirroring the close fan-out. A transport with no spawner wired
    /// (a test binding such as `testing::unrouted_binding`) has no monitor
    /// index to drain, so the call is a no-op.
    pub fn vacate(&self) {
        let Some(spawner) = self.binding.spawner() else {
            return;
        };
        let registry = spawner.actor_registry();
        let occupant = self.binding.self_mailbox();

        notify_departure(self.binding, occupant, registry.vacate_actor(occupant));
        notify_alias_departures(registry, self.binding, occupant);
    }

    /// ADR-0114 teardown (#4228): declare one inline-child `alias` folded onto
    /// the calling actor's mailbox vacated, because the child that occupied it
    /// was despawned. Drains that alias's watchers and fires one
    /// [`aether_kinds::MonitorNotice`] sent from it — the single-address form of
    /// what [`Self::vacate`] does for a whole departing cluster.
    ///
    /// Self-service like `vacate`, and narrower: an actor can only vacate an
    /// alias that is folded onto its own mailbox, so a caller cannot reach a
    /// peer's inline children. An alias that is not this actor's is a no-op
    /// `false`, as is a transport with no spawner wired
    /// (a test binding such as `testing::unrouted_binding`) — there is no
    /// monitor index to drain.
    ///
    /// Retiring the route itself is a separate, owner-staged step: the notice
    /// goes out from the despawning actor's own turn, while the route change
    /// lands through the registry owner.
    pub fn vacate_alias(&self, alias: MailboxId) -> bool {
        let Some(spawner) = self.binding.spawner() else {
            return false;
        };
        if !self.binding.mailer().registry().is_alias_to(alias, self.binding.self_mailbox()) {
            return false;
        }
        let registry = spawner.actor_registry();
        notify_departure(self.binding, alias, registry.vacate_actor(alias));
        true
    }
}
