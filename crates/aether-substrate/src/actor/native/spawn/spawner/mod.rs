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
use std::sync::{Arc, Mutex};

use aether_actor::{ActorRef, Addressable, ErasedActorRef};
use aether_data::{ActorPath, Address};
use crossbeam_channel::Receiver;

use crate::actor::registry::ActorRegistry;
use crate::chassis::builder::ReplyTarget;
use crate::config::RingCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::{
    AddressResolutionError, AdoptRefused, BootAuthority, ChildRefused, Registry, ResolvedAddress,
};
use crate::mail::{KindId, Mail, MailId, MailboxId, Source, SourceAddr};
use crate::runtime::lifecycle::FatalAborter;
use crate::scheduler::{Drainable, WakeHandle, WakeSink};

pub(super) mod commit;
pub(super) mod prepare;
mod teardown;

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
    actor_registry: Arc<ActorRegistry>,
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
    /// Issue 635 Phase 3: strong-Arc store for instanced
    /// [`Drainable`] slots spawned via the Pooled
    /// branch. Without this the slot dropped at end of `spawn_actor`
    /// and the [`WakeHandle`]'s `Weak` failed to
    /// upgrade — every wake after spawn would silently no-op.
    /// Slots live until the Spawner itself drops (chassis teardown);
    /// self-closing actors leave their slot Arc here as a small
    /// metadata leak (~80 B) that's reclaimed at teardown. Nothing an
    /// actor holds on behalf of a *peer* may ride that retention: a
    /// resource whose lifetime is the actor's own life is released on the
    /// close path (cost rows, the parent-local child key — issue 4152),
    /// never left for the teardown drain.
    ///
    /// Issue 685: each entry now also carries a [`WakeHandle`] clone
    /// so [`Self::shutdown_instanced`] can fire one wake per slot at
    /// chassis teardown — without it, a freshly-`signal_shutdown`-ed
    /// slot whose inbox is empty would never enter `run_cycle` to
    /// observe the flag.
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
}

/// One entry in [`Spawner::instanced_slots`]. Holds both the strong
/// `Arc<dyn Drainable>` (so the wake handle's `Weak` upgrades) and a
/// [`WakeHandle`] clone (so the chassis-teardown
/// path can wake the slot after signaling shutdown). Issue 685.
pub(in crate::actor::native::spawn) struct InstancedSlotEntry {
    slot: Arc<dyn Drainable>,
    wake: WakeHandle,
}
impl Spawner {
    pub fn new(
        registry: Arc<Registry>,
        actor_registry: Arc<ActorRegistry>,
        mailer: Arc<Mailer>,
        aborter: Arc<dyn FatalAborter>,
        wake_sink: WakeSink,
        ring_capacities: RingCapacities,
    ) -> Self {
        Self {
            registry,
            actor_registry,
            mailer,
            aborter,
            counter: AtomicU64::new(0),
            wake_sink,
            instanced_slots: Mutex::new(HashMap::new()),
            ring_capacities,
            authority: Mutex::new(Some(BootAuthority::new())),
        }
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
    /// The chassis builder calls this after a successful driver `Start` and
    /// immediately before returning a `PassiveChassis`. A failed `Start` never
    /// reaches it, so a chassis that never came up leaves boot's own writer
    /// intact for the unwind.
    pub(crate) fn seal(&self) -> Option<BootAuthority> {
        self.authority.lock().expect("spawner boot authority lock poisoned; fail-fast per ADR-0063").take()
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

    pub(super) fn retain_activated_slot(&self, id: MailboxId, slot: Arc<dyn Drainable>, wake: WakeHandle) {
        self.instanced_slots
            .lock()
            .expect("instanced_slots mutex poisoned; fail-fast per ADR-0063")
            .insert(id, InstancedSlotEntry { slot, wake });
    }

    /// ADR-0097: allocate the next monotonic discriminator from the same
    /// per-chassis sequence [`Subname::Counter`](super::Subname::Counter) draws on. The
    /// `spawn_sibling` host fn calls this to resolve a wasm
    /// `Subname::Counter` synchronously — it bakes the value into a
    /// `Named` subname so the spawned trampoline's `MailboxId` is known
    /// before the spawn completes (ADR-0097 §4), without double-drawing
    /// the counter at spawn time.
    pub fn next_counter(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Borrow the actor registry. Crate-private — substrate-internal
    /// dispatcher trampolines (instanced spawn close path, singleton
    /// boot path) use this to call `close_actor` / `mark_dead` /
    /// `try_claim_namespace` etc. Cap handlers reaching for the
    /// registry through `transport.spawner().actor_registry()` is
    /// the wrong shape — caps that supervise a fleet hold their own
    /// child map; caps that just send mail use the typed `ctx.actor`
    /// / `ctx.to` shortcuts. ADR-0079 supervisor-as-cap
    /// pattern.
    pub(crate) fn actor_registry(&self) -> &Arc<ActorRegistry> {
        &self.actor_registry
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

    /// Resolve a canonical or ADR-0166 short [`ActorPath`] to one live
    /// mailbox through the registry's boundary parser, keeping the registry
    /// itself behind the spawner. The chassis handle's embedder lookup
    /// forwards here.
    pub(crate) fn resolve_address(&self, address: &ActorPath) -> Result<ResolvedAddress, AddressResolutionError> {
        self.registry.resolve_address(address)
    }

    /// Type a load reply's stamped sender as the loaded actor `R`, keeping
    /// the registry itself behind the spawner. The chassis handle's
    /// `adopt_load` forwards here.
    pub(crate) fn adopt_loaded<R>(&self, sender: ErasedActorRef) -> Result<ActorRef<R>, AdoptRefused> {
        self.registry.loaded::<R>(sender)
    }

    /// Prove the child a held parent's `address` names, keeping the registry
    /// itself behind the spawner. The chassis handle's `child` forwards here.
    pub(crate) fn live_child<C: Addressable>(&self, address: &Address<C>) -> Result<ActorRef<C>, ChildRefused> {
        self.registry.live_child(address)
    }

    /// Body of the chassis handle's `send_tracked`: push `payload` to the
    /// actor `to` proves as a chassis-root mail, optionally carrying a reply
    /// target, and return the receiver that fires once its causal chain
    /// settles (ADR-0080 §6).
    ///
    /// The three chassis-root steps — mint the root id, record its `Sent`,
    /// push with lineage — are `Mailer::push_chassis_root_mail`'s, open-coded
    /// here so the push can also carry a reply target. The subscription lands
    /// between the record and the push, so the settlement cannot fire before
    /// the caller holds the receiver.
    ///
    /// # Panics
    /// Panics if the chassis boot did not install its settlement registry on
    /// the mailer — every built chassis does, before any cap boots.
    pub(crate) fn push_tracked(
        &self,
        to: ErasedActorRef,
        kind: KindId,
        payload: Vec<u8>,
        correlation: u64,
        reply: Option<ReplyTarget>,
    ) -> Receiver<()> {
        let recipient = to.id();
        let root = MailId::new(MailboxId::CHASSIS_MAILBOX_ID, correlation);
        self.mailer.record_sent(root, root, None, MailboxId::CHASSIS_MAILBOX_ID, recipient, kind);
        let settlement = self
            .mailer
            .settlement_registry()
            .expect("the chassis boot installs its settlement registry on the mailer")
            .subscribe_settlement(root);

        let mail = Mail::new(recipient, kind, payload, 1).with_lineage(Some(root), Some(root), None);
        self.mailer.push(match reply {
            Some(reply) => mail.with_reply_to(reply_source(reply)),
            None => mail,
        });
        settlement
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
