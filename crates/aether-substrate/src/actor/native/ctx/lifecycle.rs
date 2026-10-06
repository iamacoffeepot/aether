//! Retiring this actor, and watching the ones it depends on, a chain's
//! settlement, and the registry inventory.
//!
//! Issue 607 Phase 4 (ADR-0079) self-shutdown and ADR-0063 fail-fast on one
//! side; the ADR-0079 §8 monitor surface on the other — register a watch,
//! or close a single inline-child alias folded onto the caller (ADR-0114).

use aether_actor::{ErasedActorRef, HandlesKind, RegistryChanged, ReplyMode};
use aether_data::{Kind, MailId};

use crate::actor::monitor::{MonitorHandle, notify_departure};
use crate::mail::registry::{PreparedAliasRetirement, RegistrySubscription};

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// Issue 607 Phase 4a (ADR-0079): self-shutdown signal. Sets a
    /// flag the actor's dispatcher polls after each handler returns;
    /// when set, the trampoline drains any remaining inbox mail
    /// synchronously, runs `NativeActor::unwire`, and exits the
    /// dispatch loop. After exit the actor's [`MailboxId`](aether_data::MailboxId)
    /// transitions from `Live` to `Dead` in the chassis's
    /// [`ActorRegistry`](crate::ActorRegistry) and is added to `tombstones` —
    /// `spawn_child` rejects reuse of the retired full name with
    /// `SpawnError::SubnameRetired`. Its route goes `Dropped` as well, at the
    /// registry owner's next apply: `resolve_live` refuses it, the live
    /// inventory drops it, and its name is never registered again
    /// (ADR-0079 §7).
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

    /// Watch `target` (ADR-0079 §8): the calling actor is sent one
    /// [`aether_kinds::MonitorNotice`] when `target` closes. It never fails.
    ///
    /// The notice is ordinary mail whose envelope sender is the departed
    /// actor, so its handler reads `ctx.sender()` to get the same
    /// [`ErasedActorRef`] it monitored; it means state keyed by that
    /// reference is stale.
    ///
    /// - **A target that has already closed** is noticed the same way. A
    ///   reference proves its actor reached `Live`, never that it is `Live`
    ///   now (ADR-0230 §1), so the target may have closed before this call.
    ///   Its notice is then posted here, and the caller handles it after the
    ///   handler that is running returns. State the caller keys on `target`
    ///   in that handler is therefore in place when the notice arrives.
    /// - **Dropping the returned [`MonitorHandle`]** stops the watch. A
    ///   notice already posted when the handle drops still arrives, so a
    ///   notice handler does nothing when it holds no state under its
    ///   sender.
    /// - **One notice per call.** Two monitors of one target are two
    ///   registrations and receive two notices.
    ///
    /// `target` is any proven reference: an [`ErasedActorRef`], or an
    /// `ActorRef<R>` or `ProtocolRef<P>` passed as it is.
    /// Any proven target can be watched: an instanced actor, a composed or
    /// pumped root, an inline-child alias (ADR-0114 §2). A caller whose
    /// `wire` monitors before its own route is `Live` has a notice held
    /// until its birth promotes.
    pub fn monitor(&self, target: impl Into<ErasedActorRef>) -> MonitorHandle {
        MonitorHandle::register(self.binding, self.binding.self_mailbox(), target.into().id())
    }

    /// ADR-0080 §6: subscribe the calling actor to one `K` notice when the
    /// chain rooted at `root` settles. `K`'s payload is
    /// [`Settled`](aether_kinds::trace::Settled)'s single [`MailId`], the settled
    /// root, and the notice pre-fires at once when `root` has already
    /// settled. The bound `A: HandlesKind<K>` makes a notice kind this actor
    /// does not handle a compile error, rather than a gate that never opens.
    ///
    /// Returns `false`, and subscribes nothing, when the chassis wires no
    /// settlement registry; each caller decides what that means for it.
    /// Consumers: the render capture gate (`PreSettled`), the RPC server's
    /// `ReplyEnd`, the HTTP shard's `502` safety net, and the lifecycle
    /// advance reply.
    #[must_use]
    pub fn subscribe_settlement<K: Kind>(&self, root: MailId) -> bool
    where
        A: HandlesKind<K>,
    {
        self.binding.subscribe_settlement_notice(root, K::ID)
    }

    /// Subscribe the calling actor to the registry's inventory changes, as
    /// [`RegistryChanged`] wakes: one now, and one per change after the
    /// previous wake is acknowledged through
    /// [`RegistrySubscription::acknowledge`]. Dropping the returned
    /// subscription unsubscribes. Consumer: the component host, which keeps
    /// its inventory view current through it.
    pub fn subscribe_inventory(&self) -> RegistrySubscription
    where
        A: HandlesKind<RegistryChanged>,
    {
        self.binding.subscribe_inventory()
    }

    /// ADR-0241 §8: close one inline-child `alias` folded onto the calling
    /// actor's mailbox, because the child that occupied it was despawned. The
    /// alias tombstones, so a later watch on it is answered with its notice at
    /// once and a re-spawn of its key is refused before any id reaches the
    /// guest; then its watchers drain and
    /// each is sent one [`aether_kinds::MonitorNotice`] from the alias — the
    /// single-address form of what the close tail does for every alias of a
    /// closing actor.
    ///
    /// Self-service only: an actor can only close an alias that is folded
    /// onto its own mailbox, so a caller cannot reach a peer's inline
    /// children. An alias that is not this actor's is a no-op `false`.
    ///
    /// Retiring the route itself is a separate, owner-staged step: the notice
    /// goes out from the despawning actor's own turn, while the route change
    /// lands through the registry owner. The token comes from the drained
    /// retirements (`Component::drain_pending_alias_retirements`).
    pub fn close_alias(&self, alias: &PreparedAliasRetirement) -> bool {
        let registry = self.binding.mailer().registry();
        if !registry.is_alias_to(alias.alias, self.binding.self_mailbox()) {
            return false;
        }
        notify_departure(self.binding, alias.alias, registry.actor_registry().close_alias(alias.alias));
        true
    }
}
