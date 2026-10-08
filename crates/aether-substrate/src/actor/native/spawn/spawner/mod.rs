//! The chassis-level spawn engine both builders funnel through.
//!
//! One [`Spawner`] per chassis, cloned as an `Arc` into every
//! [`NativeBinding`](crate::actor::native::binding::NativeBinding) so a
//! handler's `spawn_child` reaches it without explicit plumbing. What it
//! holds is the shared state a birth touches; what it does is split by
//! phase into the siblings beside it — `prepare` resolves identity and
//! constructs the actor without a single shared write, `commit` takes
//! whichever of the two routes the ADR-0165 seal leaves open, and
//! `teardown` walks every slot it ever retained at chassis shutdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use aether_actor::{ActorRef, ErasedActorRef, Instanced};
use aether_data::{ErasedActorPath, LoadName};
use crossbeam_channel::Receiver;

use crate::actor::registry::ActorRegistry;
use crate::chassis::builder::{ReplyTarget, RootPusher};
use crate::config::RingCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::{
    AddressResolutionError, AdoptRefused, BootAuthority, ChildRefused, Registry, ResolvedAddress,
};
use crate::mail::{KindId, Mail, MailId, MailboxId, Source, SourceAddr};
use crate::runtime::lifecycle::FatalAborter;
use crate::runtime::wire_root::WireRoot;
use crate::scheduler::{Drainable, WakeHandle, WakeSink};

#[cfg(any(test, feature = "test-support"))]
mod close_wait;
pub(super) mod commit;
pub(super) mod prepare;
#[cfg(any(test, feature = "test-support"))]
mod registry_barrier;
mod teardown;
#[cfg(any(test, feature = "test-support"))]
mod wire_wait;

/// The dispatch `Source` an embedder's [`ReplyTarget`] names: the push's
/// `reply_to`, read at the push and nowhere else.
fn reply_source(reply: ReplyTarget) -> Source {
    match reply {
        ReplyTarget::Session { session, correlation } => {
            Source::with_correlation(SourceAddr::Session(session), correlation)
        }
        ReplyTarget::Actor { to, correlation } => Source::with_correlation(SourceAddr::Component(to.id()), correlation),
    }
}

/// Chassis-level spawn machinery (Phase 3). One per chassis; cloned as
/// `Arc<Spawner>` into every [`NativeBinding`](crate::actor::native::binding::NativeBinding) so per-handler
/// `NativeCtx::spawn_child` can reach it without explicit plumbing.
pub struct Spawner {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    aborter: Arc<dyn FatalAborter>,
    /// Monotonic counter for [`Subname::Counter`](super::Subname::Counter). Per-Spawner so each
    /// chassis runs its own sequence; not shared across substrates.
    counter: AtomicU64,
    /// Issue 635 PR C: chassis worker pool's wake sink — the ready-queue
    /// sender bundled with the spin/park coordinator (iamacoffeepot/aether#1064).
    /// Cloned into [`WakeHandle`]s when the Pooled spawn branch lands a
    /// slot.
    wake_sink: WakeSink,
    /// The pooled instanced actors that are born and have not closed, one
    /// entry each. An entry holds the strong `Arc<dyn Drainable>` that
    /// keeps the actor's slot alive between dispatches (a [`WakeHandle`]
    /// and a seize handle hold only a `Weak`, so without it every wake
    /// after the birth would find nothing to upgrade), a [`WakeHandle`]
    /// clone so [`Self::shutdown_instanced`] can schedule a quiet slot
    /// after signalling it (issue 685), and in a test build the
    /// subscription to the birth's wire root.
    ///
    /// [`Self::retain_activated_slot`] inserts the entry at birth. The
    /// actor's own close cycle removes it through
    /// [`Self::release_closed_slot`], after the registry close and before
    /// the close-done signal, so a closed actor leaves nothing here and its
    /// slot, with its rings and its binding, is freed once the worker
    /// running that cycle returns (issue #7402). Chassis teardown drains
    /// what is left, which is only the actors still open.
    ///
    /// The test-support `Spawner::await_closed` finds a still-open actor's
    /// slot here to install its close-done sender (issue #7074); for an
    /// actor that has already closed it reads the actor registry instead.
    pub(in crate::actor::native::spawn) instanced_slots: Mutex<HashMap<MailboxId, InstancedSlotEntry>>,
    /// Issue 1990: the per-actor ring capacities resolved at chassis
    /// boot. Every actor spawned through [`Self::build`] seeds its
    /// `ActorLogRing` / `ActorTraceRing` at these caps right after
    /// `ActorSlots::new()`, so the chassis-wide knob reaches instanced
    /// actors (and the wasm trampolines that spawn through this same
    /// funnel) without per-spawn plumbing.
    ring_capacities: RingCapacities,
    /// iamacoffeepot/aether#4156: proof that the eager commit half may write
    /// the registry directly, and — since iamacoffeepot/aether#4167 — the last
    /// such proof still in circulation once boot ends. The `Spawner` is built
    /// once in `boot_passives` and outlives boot behind an `Arc`, so it is the
    /// one holder whose token would otherwise let a post-seal caller name the
    /// direct writer. [`Spawner::seal`] takes it; after that
    /// [`Spawner::commit`] cannot produce a `&BootAuthority` and therefore
    /// cannot name `Registry::apply_one` at all, and every birth it runs goes
    /// through the ADR-0165 owner like a staged child birth.
    ///
    /// `Mutex<Option<_>>` rather than a `OnceLock`-shaped take because the
    /// seal runs against a shared `&Spawner` and `OnceLock` can only be
    /// emptied through `&mut self`. Taking under the lock also makes the
    /// sealed state the *absence* of a token rather than a flag sitting
    /// beside a live one.
    authority: Mutex<Option<BootAuthority>>,
    /// ADR-0244: the boot's held wire root, opened by `boot_passives` right
    /// after this `Spawner` is built and kept beside [`Self::authority`] for
    /// exactly as long. Every pre-seal birth's `wire` — the Pass 3 wire, a
    /// pre-seal direct commit, a driver `Start` pumped actor — runs under it,
    /// and [`Self::seal`] drops it with the authority, releasing the hold that
    /// kept it open while boot could still add sends to it.
    boot_wire: Mutex<Option<WireRoot>>,
}

/// One entry in [`Spawner::instanced_slots`]. Holds both the strong
/// `Arc<dyn Drainable>` (so the wake handle's `Weak` upgrades) and a
/// [`WakeHandle`] clone (so the chassis-teardown
/// path can wake the slot after signaling shutdown). Issue 685. The entry
/// lasts from the actor's birth to the end of its close cycle.
pub(in crate::actor::native::spawn) struct InstancedSlotEntry {
    pub(in crate::actor::native::spawn) slot: Arc<dyn Drainable>,
    wake: WakeHandle,
    /// Issue #7120: the subscription to the birth's wire root (ADR-0244 §7)
    /// that the test-support `Spawner::await_wire_settled` consumes, kept on
    /// the one row each born actor already has.
    #[cfg(any(test, feature = "test-support"))]
    wire_settled: wire_wait::WireSettled,
}
impl Spawner {
    pub fn new(
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
        aborter: Arc<dyn FatalAborter>,
        wake_sink: WakeSink,
        ring_capacities: RingCapacities,
    ) -> Self {
        Self {
            registry,
            mailer,
            aborter,
            counter: AtomicU64::new(0),
            wake_sink,
            instanced_slots: Mutex::new(HashMap::new()),
            ring_capacities,
            authority: Mutex::new(Some(BootAuthority::new())),
            boot_wire: Mutex::new(None),
        }
    }

    /// Open the boot's wire root (ADR-0244), held until [`Self::seal`].
    ///
    /// # Panics
    /// Panics if the boot wire is already open.
    pub(crate) fn open_boot_wire(&self) {
        let previous = self.lock_boot_wire().replace(WireRoot::open(&self.mailer));
        assert!(previous.is_none(), "a chassis opens one boot wire root");
    }

    /// Subscribe to the boot wire root's settlement: the receiver fires once
    /// [`Self::seal`] has released its hold and every mail a pre-seal `wire`
    /// sent, and everything those mails caused, has been handled.
    ///
    /// # Panics
    /// Panics if the boot wire is not open, which it is from
    /// [`Self::open_boot_wire`] until the seal.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn subscribe_boot_wire(&self) -> Receiver<()> {
        self.lock_boot_wire().as_ref().expect("the boot wire root is open until the seal").subscribe(&self.mailer)
    }

    /// The boot's wire root while boot is still open, `None` once
    /// [`Self::seal`] has dropped it: a birth after the seal is an embedder's
    /// and opens its own.
    pub(crate) fn boot_wire_root(&self) -> Option<MailId> {
        self.lock_boot_wire().as_ref().map(WireRoot::root)
    }

    fn lock_boot_wire(&self) -> MutexGuard<'_, Option<WireRoot>> {
        self.boot_wire.lock().expect("spawner boot wire lock poisoned; fail-fast per ADR-0063")
    }

    /// Install the ADR-0165 runtime seal: take the boot authority out of
    /// circulation so nothing can reach the registry's direct write path
    /// again.
    ///
    /// Returns the token so the caller owns the moment it dies; dropping the
    /// return value is the whole effect. Idempotent — a second call finds
    /// `None`, which is what makes a re-entered teardown or a double-sealed
    /// test harmless.
    ///
    /// The boot's wire root (ADR-0244) leaves with the token: no birth after
    /// the seal runs under it, so its hold is released here and the root
    /// settles once the boot's `wire` mail has been handled.
    ///
    /// The chassis builder calls this after a successful driver `Start` and
    /// immediately before returning a `PassiveChassis`. A failed `Start` never
    /// reaches it, so a chassis that never came up leaves boot's own writer
    /// intact for the unwind.
    pub(crate) fn seal(&self) -> Option<BootAuthority> {
        let authority =
            self.authority.lock().expect("spawner boot authority lock poisoned; fail-fast per ADR-0063").take();
        drop(self.lock_boot_wire().take());
        authority
    }

    /// Borrow the chassis worker pool's wake sink (ready-queue sender +
    /// spin/park coordinator). The Pooled instanced spawn branch clones
    /// it into each slot's [`WakeHandle`].
    pub(crate) fn wake_sink(&self) -> &WakeSink {
        &self.wake_sink
    }

    /// The per-actor ring capacities resolved at chassis boot (issue
    /// 1990). The chassis builder's singleton cap-claim path reads these
    /// off the shared `Spawner` so it seeds its `ActorSlots` rings at the
    /// same caps the instanced spawn funnel applies — one source of
    /// truth for both slot sites.
    pub(crate) fn ring_capacities(&self) -> RingCapacities {
        self.ring_capacities
    }

    /// Keep a pooled instanced actor's slot and wake handle for teardown.
    ///
    /// `wire_root` is the root the birth's `wire` ran under when the birth
    /// opened its own (ADR-0244), still held open by the caller. A test
    /// build subscribes to it here, before the hold is released, so the
    /// test-support `Spawner::await_wire_settled` cannot miss the settle.
    /// `None` is a birth whose `wire` ran under the boot's root, or under
    /// none.
    pub(super) fn retain_activated_slot(
        &self,
        id: MailboxId,
        slot: Arc<dyn Drainable>,
        wake: WakeHandle,
        wire_root: Option<&WireRoot>,
    ) {
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = wire_root;
        let entry = InstancedSlotEntry {
            slot,
            wake,
            #[cfg(any(test, feature = "test-support"))]
            wire_settled: wire_wait::WireSettled::subscribe(wire_root, &self.mailer),
        };
        self.instanced_slots.lock().expect("instanced_slots mutex poisoned; fail-fast per ADR-0063").insert(id, entry);
    }

    /// Give up a pooled instanced actor's entry once its close cycle has
    /// run: the inverse of [`Self::retain_activated_slot`]. The caller is
    /// the closing slot itself, reached by a worker that holds its own
    /// strong reference for the length of the cycle, so the slot is freed
    /// when that worker returns and never under the caller.
    ///
    /// An id with no entry is not an error: chassis teardown may have
    /// drained it first, and a composed singleton was never retained.
    pub(crate) fn release_closed_slot(&self, id: MailboxId) {
        let released =
            self.instanced_slots.lock().expect("instanced_slots mutex poisoned; fail-fast per ADR-0063").remove(&id);

        drop(released);
    }

    /// Allocate the next monotonic discriminator from the same per-chassis
    /// sequence [`Subname::Counter`](super::Subname::Counter) draws on. The
    /// inline-child spawn host fns (ADR-0114) call this to resolve a wasm
    /// `Subname::Counter` synchronously — they bake the value into the
    /// alias's subname so its `MailboxId` is known within the guest call.
    pub fn next_counter(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Borrow the actor registry, which the route registry owns
    /// ([`Registry::actor_registry`]). Crate-private — substrate-internal
    /// dispatcher trampolines (instanced spawn close path, singleton
    /// boot path) use this to call `close_actor` / `mark_dead` /
    /// `is_tombstoned` etc. Cap handlers reaching for the
    /// registry through `transport.spawner().actor_registry()` is
    /// the wrong shape — caps that supervise a fleet hold their own
    /// child map; caps that just send mail use the flat `ctx.send::<R>`
    /// / `ctx.send_to` verbs. ADR-0079 supervisor-as-cap pattern.
    pub(crate) fn actor_registry(&self) -> &Arc<ActorRegistry> {
        self.registry.actor_registry()
    }

    /// The chassis mailer, cloned into each booted [`NativeBinding`](crate::actor::native::binding::NativeBinding).
    /// ADR-0161 slice R4: the passive pumped-actor boot
    /// ([`crate::chassis::builder::PassiveChassis::boot_pumped_actor`])
    /// reaches it through the `Spawner` the `PassiveChassis` holds, since a
    /// no-driver chassis has no [`crate::chassis::ctx::ChassisCtx`] post-boot
    /// to source it from.
    pub(crate) fn mailer(&self) -> &Arc<Mailer> {
        &self.mailer
    }

    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Resolve a canonical or ADR-0166 short [`ErasedActorPath`] to one live
    /// mailbox through the registry's boundary parser, keeping the registry
    /// itself behind the spawner. The chassis handle's embedder lookup
    /// forwards here.
    pub(crate) fn resolve_address(&self, address: &ErasedActorPath) -> Result<ResolvedAddress, AddressResolutionError> {
        self.registry.resolve_address(address)
    }

    /// Type a load reply's stamped sender as the loaded actor `R`, keeping
    /// the registry itself behind the spawner. The chassis handle's
    /// `adopt_load` forwards here.
    pub(crate) fn adopt_loaded<R>(&self, sender: ErasedActorRef) -> Result<ActorRef<R>, AdoptRefused> {
        self.registry.loaded::<R>(sender)
    }

    /// Prove the child at `key` beneath a held `parent`, keeping the registry
    /// itself behind the spawner. The chassis handle's `child` forwards here.
    pub(crate) fn live_child<C: Instanced>(
        &self,
        parent: ErasedActorRef,
        key: LoadName,
    ) -> Result<ActorRef<C>, ChildRefused> {
        self.registry.live_child::<C>(parent, key)
    }

    /// Body of the chassis handle's `send_tracked`: push `payload` to the
    /// actor `to` proves as a chassis-root mail, optionally carrying a reply
    /// target, and return the minted root beside the receiver that fires once
    /// its causal chain settles (ADR-0080 §6).
    ///
    /// The root is minted from the mailer's one chassis-root counter. The
    /// subscription lands between the mint, which records the `Sent`, and the
    /// push, so the settlement cannot fire before the caller holds the
    /// receiver.
    ///
    /// # Panics
    /// Panics if the chassis boot did not install its settlement registry on
    /// the mailer — every built chassis does, before any cap boots.
    pub(crate) fn push_tracked(
        &self,
        to: ErasedActorRef,
        kind: KindId,
        payload: Vec<u8>,
        reply: Option<ReplyTarget>,
    ) -> (MailId, Receiver<()>) {
        let minted = self.mailer.mint_chassis_root(to.id(), kind);
        let settlement = self
            .mailer
            .settlement_registry()
            .expect("the chassis boot installs its settlement registry on the mailer")
            .subscribe_settlement(minted.id());

        let root = self.mailer.push_minted_root(minted, payload, reply.map_or(Source::NONE, reply_source));
        (root, settlement)
    }

    /// Body of the chassis handle's `root_pusher`: the chassis-root door to
    /// the actor `to` proves, minting from this chassis's mailer.
    pub(crate) fn root_pusher<R>(&self, to: ActorRef<R>) -> RootPusher<R> {
        RootPusher::new(to, Arc::clone(&self.mailer))
    }

    /// Body of the chassis handle's `send_for_reply`: push `payload` to the
    /// actor `to` proves, untracked, with its reply routed to `reply`.
    pub(crate) fn push_for_reply(&self, to: ErasedActorRef, kind: KindId, payload: Vec<u8>, reply: ReplyTarget) {
        self.mailer.push(Mail::new(to.id(), kind, payload, 1).with_reply_to(reply_source(reply)));
    }

    /// The chassis fatal-abort handle, cloned into each booted
    /// [`NativeBinding`](crate::actor::native::binding::NativeBinding). Reached by the passive pumped-actor boot (ADR-0161
    /// slice R4) the same way as [`Self::mailer`].
    pub(crate) fn aborter(&self) -> &Arc<dyn FatalAborter> {
        &self.aborter
    }
}
