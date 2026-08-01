//! Native spawn lifecycle for instanced actors (ADR-0079, ADR-0165).
//!
//! Chassis and transitional embedder callers use the eager
//! [`SpawnBuilder::finish`] bridge. Handler callers use
//! [`HandlerSpawnBuilder::stage`] or [`HandlerSpawnBuilder::stage_with`]:
//! validate and initialize on the handler thread, append an ordered
//! `PreparedSpawnCommit` to that turn's outbound work, and return a local
//! reservation receipt without publishing global state. The registry owner
//! then authoritatively reserves `Starting`; the scheduler home wires the
//! initialized actor; an activation barrier promotes it to `Live`; and the
//! finalizer delivers the typed ADR-0093 `TaskDone` result to the parent.
//!
//! Initialization failure drops partial state synchronously before anything
//! is staged. Owner-time conflicts and activation failures drop state at its
//! scheduler home and complete the same typed deferred result exactly once.

use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use aether_actor::{HandlesKind, Instanced, NamespaceError, validate_namespace_segment};
use aether_data::{ActorId, Kind, Tag, fold_lineage, with_tag};
use aether_kinds::trace::Nanos;

use super::reservation::ChildReservationKey;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::actor::native::offload::blocking::IntoDeferredReply;
use crate::actor::native::slot::dispatcher::DispatcherSlot;
use crate::actor::native::{DispatchId, ExportedHandles, NativeActor, NativeCtx, NativeInitCtx};
use crate::actor::registry::ActorRegistry;
use crate::chassis::ctx::{MailboxWakeSlot, RelayOutcome, relay_or_transfer};
use crate::chassis::error::BootError;
use crate::chassis::settlement::{TerminalDisposition, WaitOutcome, await_internal_signal};
use crate::config::RingCapacities;
use crate::mail::cost::{CostCell, CostCells};
use crate::mail::mailer::Mailer;
use crate::mail::registry::OwnedDispatch;
use crate::mail::registry::effect::{
    EffectBatch, PreparedCostCells, PreparedMail, PreparedRoute, PreparedSpawnCommit, RegistryEffect,
};
use crate::mail::registry::{BootAuthority, NameConflict, Registry};
use crate::mail::{KindId, Mail, MailId, MailRef, MailboxId, Source};
use crate::runtime::effect_chain::{EffectChain, OrderingDevice, Uncaused};
use crate::runtime::lifecycle::{FatalAbortRecord, FatalAborter};
use crate::runtime::trace::SettlementHold;
use crate::scheduler::Drainable;
use crate::scheduler::SeizeHandle;
use crate::scheduler::WakeHandle;
use crate::scheduler::WakeSink;
use aether_actor::local::ActorSlots;
use aether_actor::log::ActorLogRing;

use crate::actor::native::local;
use aether_actor::trace::ActorTraceRing;
use std::sync::Weak;
use std::time::Duration;

/// The spawn-subname vocabulary, re-exported from `aether-actor`
/// (ADR-0097). It's shared between native `spawn_child` and the FFI
/// guest's `WasmCtx::spawn_child`, so it lives in the actor SDK both
/// transports depend on; native call sites import it from this path
/// unchanged. The full mailbox name is `"{A::NAMESPACE}:{subname}"`,
/// hashed deterministically (ADR-0029) to the returned `MailboxId`.
pub use aether_actor::Subname;

/// Failure modes for native actor spawning.
///
/// [`HandlerSpawnBuilder::stage`] and [`HandlerSpawnBuilder::stage_with`]
/// return local validation, parent-reservation, and initialization failures
/// synchronously. Global namespace, tombstone, route, storage, and owner
/// decisions are authoritative only when the registry owner applies the staged
/// birth; those failures arrive later on the [`SpawnOutcome::result`] of the
/// matching ADR-0093 `TaskDone<SpawnOutcome, _>`. The transitional eager
/// [`SpawnBuilder::finish`] path returns all of its failures directly.
#[derive(Debug)]
pub enum SpawnError {
    /// Subname is empty, contains `:`, has control / whitespace
    /// chars, or exceeds the byte cap. See
    /// [`NamespaceError`].
    SubnameInvalid(NamespaceError),
    /// `A::NAMESPACE` is already owned by a different `TypeId`. Trips
    /// when an `Instanced` type tries to spawn under a namespace a
    /// `Singleton` already owns (or vice versa). ADR-0079 unique-owner
    /// invariant.
    NamespaceOwnedByOtherType { namespace: &'static str, owning_type: TypeId },
    /// The full name was previously live and has been retired. Names
    /// don't recycle within a substrate's lifetime (ADR-0079 §Drop /
    /// lifecycle); pick a different subname.
    SubnameRetired { full_name: String },
    /// The full name is currently bound to a live mailbox.
    SubnameInUse { full_name: String },
    /// `A::init` returned an error. The actor's partial state dropped
    /// before this returns; no dispatcher thread was spawned.
    InitFailed(BootError),
    /// The registry owner closed before it could authoritatively apply the
    /// staged birth.
    OwnerClosed,
    /// Storage or cost reservation rejected the prepared activation.
    ActivationRejected,
    /// A post-seal external birth was accepted by the registry owner and then
    /// nothing decided it within the spawn path's patience budget (30 s).
    /// Reachable only from a wedged worker pool — the activation runs at its
    /// scheduler home, so a pool that never schedules it leaves the caller
    /// with no answer. Reported rather than fatal: the caller is an external
    /// thread that can log it, retry, or tear the chassis down.
    BirthWedged { mailbox_id: MailboxId, waited: Duration },
}

/// How long a post-seal external spawn waits for the birth it submitted to be
/// decided. It covers both legs — the owner accepting the batch and the
/// activation reaching `Live` at its scheduler home — which take two pool
/// turns on a healthy substrate, so anything approaching this budget is a
/// wedged pool rather than a slow one.
const BIRTH_PATIENCE: Duration = Duration::from_secs(30);

/// Deterministic result returned when a handler has locally prepared and
/// staged a child birth. It names a reservation, not proof that the child is
/// live; the authoritative result arrives as a later `TaskDone`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnReceipt {
    pub mailbox_id: MailboxId,
    pub canonical_name: Arc<str>,
    pub completion: DispatchId,
}

/// The authoritative fate of one staged child birth, delivered through the
/// ADR-0093 task completion path once the registry owner has decided it.
///
/// Self-identifying on **both** arms: `mailbox_id` and `canonical_name` name
/// the child the handler staged whether or not it reached Live, so a completion
/// handler correlates the result without a hand-rolled context struct whose
/// only job was carrying an id back. `result` is `Ok(())` after the child is
/// published Live and catch-up is armed, and the precise [`SpawnError`]
/// otherwise.
#[derive(Debug)]
pub struct SpawnOutcome {
    pub mailbox_id: MailboxId,
    pub canonical_name: Arc<str>,
    pub result: Result<(), SpawnError>,
}

/// Chassis-level spawn machinery (Phase 3). One per chassis; cloned as
/// `Arc<Spawner>` into every [`NativeBinding`] so per-handler
/// `NativeCtx::spawn_child` can reach it without explicit plumbing.
pub struct Spawner {
    registry: Arc<Registry>,
    actor_registry: Arc<ActorRegistry>,
    mailer: Arc<Mailer>,
    aborter: Arc<dyn FatalAborter>,
    /// Monotonic counter for [`Subname::Counter`]. Per-Spawner so each
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
    instanced_slots: Mutex<HashMap<MailboxId, InstancedSlotEntry>>,
    /// Issue 1990: the per-actor ring capacities resolved at chassis
    /// boot. Every actor spawned through [`Self::spawn_actor`] seeds its
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
struct InstancedSlotEntry {
    slot: Arc<dyn Drainable>,
    wake: WakeHandle,
}

/// Identity resolved before construction starts. The canonical name is a
/// display/reverse-map value; `id` remains the lineage-folded route key.
pub(super) struct SpawnIdentity {
    id: MailboxId,
    carry: u64,
    canonical_name: Arc<str>,
    subname: String,
}

/// Private prepared birth. It deliberately owns the initialized state rather
/// than committing a storage representation to the builder API.
pub(super) struct StagedActor<A: NativeActor> {
    identity: SpawnIdentity,
    sender: mpsc::Sender<Envelope>,
    transport: Arc<NativeBinding>,
    slots: Box<ActorSlots>,
    state: A::State,
    after_init: Vec<Envelope>,
}

struct SpawnCommit {
    mailbox_id: MailboxId,
    canonical_name: String,
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
    /// per-chassis sequence [`Subname::Counter`] draws on. The
    /// `spawn_sibling` host fn calls this to resolve a wasm
    /// `Subname::Counter` synchronously — it bakes the value into a
    /// `Named` subname so the spawned trampoline's `MailboxId` is known
    /// before the spawn completes (ADR-0097 §4), without double-drawing
    /// the counter at spawn time.
    pub fn next_counter(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Issue 685: walk every spawned instanced slot, signal shutdown
    /// on its binding, fire one wake so a pool worker picks it up and
    /// runs the close path (drain residual → `unwire` → registry
    /// close + monitor fan-out), then wait per-slot on a one-shot
    /// completion channel until every slot has finished or `timeout`
    /// elapses.
    ///
    /// Called from [`crate::chassis::builder::BootedPassives::shutdown_in_place`]
    /// before the singleton shutdowns walk. The ordering matters:
    /// spawned actors close *first* so their `MonitorNotice` mail
    /// reaches singleton watchers while they're still alive. The
    /// pool stays alive through this method (it drops via the
    /// `_pool: PoolHandle` field on `BootedPassives` which has a later
    /// drop order than the explicit `shutdown_in_place` call), so
    /// workers can drain the close cycles we just queued.
    ///
    /// Issue 714: the original implementation polled
    /// [`Drainable::is_closed`] every 2 ms with a
    /// `timeout`-bounded loop. Under nextest contention the worker that
    /// observed the wake could be scheduled out long enough that the
    /// 2 s deadline elapsed before the close cycle ran, surfacing as
    /// the `chassis_teardown_runs_unwire` flake. The waker now installs a
    /// one-shot `crossbeam_channel::bounded(1)` per entry; the slot's
    /// close cycle fires it after `unwire` + registry close land, so
    /// teardown wakes the instant the cycle settles instead of polling.
    ///
    /// Issue #1305: each close-done receiver is waited on via
    /// [`await_internal_signal`] with escalating patience rather than a
    /// bare wall-clock `recv_timeout`. A genuinely wedged close cycle is
    /// unrecoverable — `unwire` never ran, so teardown invariants are
    /// already corrupt — so the disposition is `Abort` in release
    /// (route the wedge through the Spawner's
    /// [`FatalAborter`]) and `Panic` in test/debug (so #1295's
    /// assertion fails attributably at the gate site instead of as a
    /// downstream `0 != 1`). The old silent `warn!`-and-return-anyway
    /// path that left an un-closed actor is gone.
    ///
    /// `round_budget` is the per-round patience interval (the log
    /// cadence); `cumulative_cap` is the total patience per slot before
    /// declaring a wedge.
    ///
    /// Issue #4193: `abort_record` is the chassis's [`FatalAbortRecord`],
    /// and it is what stops this gate laundering a handler panic into a
    /// bare timeout. A panicking handler escalates through the pool
    /// worker's [`FatalAborter`]; under [`crate::runtime::lifecycle::PanicAborter`]
    /// that unwinds the worker, so the slot it was mid-turn on never
    /// fires close-done and every remaining slot waits out `cumulative_cap`
    /// — five minutes by default, longer than any test-runner ceiling, so
    /// the run reports a truncated hang and the panic that caused it never
    /// reaches the failure. Watching the record makes the gate report the
    /// abort reason instead, at the moment it looks.
    pub(crate) fn shutdown_instanced(
        &self,
        round_budget: Duration,
        cumulative_cap: Duration,
        abort_record: &FatalAbortRecord,
    ) {
        // Issue #2509: retain the slot's `MailboxId` alongside its entry
        // (previously dropped as `_id`) so a genuine teardown wedge names
        // the actor whose close cycle failed rather than a bare
        // gate-name panic.
        let entries: Vec<(MailboxId, InstancedSlotEntry)> = {
            let mut guard =
                self.instanced_slots.lock().expect("instanced_slots mutex poisoned; fail-fast per ADR-0063");
            guard.drain().collect()
        };
        if entries.is_empty() {
            return;
        }
        // Wire one (tx, rx) per entry up-front. Installing the tx on
        // the slot before signalling shutdown ensures the close cycle
        // sees the sender to fire — even if the worker enters the
        // close path before `signal_shutdown` returns control. The
        // slot's `set_close_done_tx` fast-paths an already-closed slot
        // by firing immediately, so there's no race window where the
        // close cycle ran without seeing the tx.
        let mut waiters: Vec<crossbeam_channel::Receiver<()>> = Vec::with_capacity(entries.len());
        for (_id, entry) in &entries {
            let (tx, rx) = crossbeam_channel::bounded::<()>(1);
            entry.slot.set_close_done_tx(tx);
            waiters.push(rx);
            entry.slot.signal_shutdown();
            // Shutdown wake: schedule the slot so the worker observes
            // the shutdown signal. The CAS-win bool is meaningful only
            // for callers wiring up first-time scheduling races; here
            // we just need *some* worker to pick the slot up.
            let _ = entry.wake.wake();
        }
        // `Panic` in test/debug (attributable failure at the gate),
        // `Abort` in release (the wedge is unrecoverable — route it
        // through the Spawner's aborter). The helper diverges itself on
        // `Panic`; on `Abort` it hands back the wedge for us to abort.
        let disposition = if cfg!(debug_assertions) {
            TerminalDisposition::Panic
        } else {
            TerminalDisposition::Abort
        };
        for ((id, _entry), rx) in entries.iter().zip(&waiters) {
            // Issue #2509: name the wedged slot in the gate label so a
            // teardown wedge panic/abort points at the actor whose close
            // cycle failed (e.g. `shutdown_instanced.close_done[mbx-…]`)
            // rather than the bare `shutdown_instanced.close_done`.
            let gate = format!("shutdown_instanced.close_done[{id}]");
            match await_internal_signal(rx, &gate, round_budget, cumulative_cap, disposition, Some(abort_record)) {
                WaitOutcome::Settled => {}
                WaitOutcome::Wedged(wedge) => {
                    // `Abort` disposition (release): the close cycle
                    // never ran `unwire`; teardown invariants are
                    // corrupt and unrecoverable. Route through the
                    // Spawner's aborter — diverges.
                    self.aborter.abort(wedge.reason());
                }
            }
        }
    }

    /// Borrow the actor registry. Crate-private — substrate-internal
    /// dispatcher trampolines (instanced spawn close path, singleton
    /// boot path) use this to call `close_actor` / `mark_dead` /
    /// `try_claim_namespace` etc. Cap handlers reaching for the
    /// registry through `transport.spawner().actor_registry()` is
    /// the wrong shape — caps that supervise a fleet hold their own
    /// child map; caps that just send mail use the typed `ctx.actor`
    /// / `ctx.resolve_actor` shortcuts. ADR-0079 supervisor-as-cap
    /// pattern.
    pub(crate) fn actor_registry(&self) -> &Arc<ActorRegistry> {
        &self.actor_registry
    }

    /// The chassis mailer, cloned into each booted [`NativeBinding`].
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

    /// The chassis fatal-abort handle, cloned into each booted
    /// [`NativeBinding`]. Reached by the passive pumped-actor boot (ADR-0161
    /// slice R4) the same way as [`Self::mailer`].
    pub(crate) fn aborter(&self) -> &Arc<dyn FatalAborter> {
        &self.aborter
    }

    /// Resolve identity and perform the current namespace and tombstone
    /// preflight before actor construction. Named-subname and typed-parent
    /// gates run in `SpawnBuilder` before this can allocate a counter.
    fn prepare_identity<A>(
        &self,
        subname: Subname<'_>,
        parent: Option<&ActorRuntimeIdentity>,
    ) -> Result<SpawnIdentity, SpawnError>
    where
        A: Instanced + NativeActor,
    {
        // 1. Resolve subname → string.
        let subname = match subname {
            Subname::Counter => self.counter.fetch_add(1, Ordering::Relaxed).to_string(),
            Subname::Named(s) => s.to_owned(),
        };
        validate_namespace_segment(&subname).map_err(SpawnError::SubnameInvalid)?;

        // Compute the lineage carry, id, and rendered name (ADR-0099
        //    §3). The child's `ActorId` is its instanced node,
        //    `hash(NAMESPACE:subname)`. Under a parent the carry folds
        //    that node onto the parent's carry and the id is the lineage
        //    fold — `MailboxId = hash(name)` no longer holds, so the id
        //    is taken from the fold and the rendered name nests under the
        //    parent's registered name. Top-level (no parent) is the
        //    depth-1 fixed point: the node is the root of its own
        //    lineage, so it keeps the flat `{NAMESPACE}:{subname}` id.
        let child_actor = ActorId::instanced(A::NAMESPACE, &subname);
        let (carry, full_name) = parent.map_or_else(
            || (child_actor.0, Arc::from(format!("{}:{}", A::NAMESPACE, subname))),
            |parent| {
                let carry = fold_lineage(parent.carry(), child_actor);
                let name: Arc<str> = Arc::from(format!("{}/{}:{}", parent.canonical_name(), A::NAMESPACE, subname));
                (carry, name)
            },
        );
        let id = MailboxId(with_tag(Tag::Mailbox, carry));
        Ok(SpawnIdentity { id, carry, canonical_name: full_name, subname })
    }

    /// Legacy eager preflight. Handler staging deliberately uses only
    /// [`Self::prepare_identity`]; namespace ownership and liveness are global
    /// facts and therefore move to owner-time activation reservation.
    fn preflight<A>(
        &self,
        subname: Subname<'_>,
        parent: Option<&ActorRuntimeIdentity>,
    ) -> Result<SpawnIdentity, SpawnError>
    where
        A: Instanced + NativeActor,
    {
        let identity = self.prepare_identity::<A>(subname, parent)?;
        if let Err(owning) = self.actor_registry.try_claim_namespace(A::NAMESPACE, TypeId::of::<A>()) {
            return Err(SpawnError::NamespaceOwnedByOtherType { namespace: A::NAMESPACE, owning_type: owning });
        }
        if self.actor_registry.is_tombstoned(identity.id) {
            return Err(SpawnError::SubnameRetired { full_name: identity.canonical_name.to_string() });
        }
        Ok(identity)
    }

    /// Construct all actor-local state with no builder or context borrow.
    fn build<A>(
        self: &Arc<Self>,
        identity: SpawnIdentity,
        config: A::Config,
        params: A::Params,
        after_init: Vec<Envelope>,
    ) -> Result<StagedActor<A>, SpawnError>
    where
        A: Instanced + NativeActor,
    {
        let SpawnIdentity { id, carry, canonical_name, subname } = identity;

        // Construct + init on caller's thread. Build the inbox pair
        // up-front so init may publish its self-id (`NativeInitCtx::self_id`
        // reads the binding's `self_mailbox`, which is this folded `id`);
        // the spawn thread doesn't exist yet.
        let (tx, rx) = mpsc::channel::<Envelope>();

        let transport = Arc::new(NativeBinding::new::<A>(
            Arc::clone(&self.mailer),
            id,
            // The child's lineage carry — its descendants fold onto it.
            carry,
            Arc::clone(&canonical_name),
            Arc::clone(&self.aborter),
            // Pass the chassis's `Spawner` through so the spawned
            // actor can in turn `ctx.spawn_child` from its own
            // handlers.
            Some(Arc::clone(self)),
        ));
        transport.install_inbox(rx);

        // Per-actor scratch storage (issue 582 / ADR-0074). Stamped
        // into TLS via `local::with_stamped` for the duration of
        // `init` and each handler dispatch so library code inside
        // the actor (e.g., the issue-581 log buffer, `Local<T>`
        // slots) can reach `Local::with_mut` without threading a
        // ctx through. Mirrors the singleton path in
        // `chassis::builder::make_native_actor_boot` (issue 672).
        let slots = Box::new(ActorSlots::new());
        // Issue 1990: seed the two per-actor rings at the chassis-wide
        // configured capacities before any handler dispatch, so the
        // first `Local::with_mut::<Ring>` finds them instead of building
        // the const-`Default` ring.
        slots.seed(ActorLogRing::with_capacity(self.ring_capacities.log));
        slots.seed(ActorTraceRing::with_growth(self.ring_capacities.trace, self.ring_capacities.trace_max));

        let state = {
            // Instanced actors don't publish driver-facing sub-handles
            // today — Phase 4+ may revisit. Pass a throwaway
            // ExportedHandles to keep the init-ctx shape uniform with
            // the singleton path.
            let mut throwaway_handles = ExportedHandles::new();
            let mut init_ctx = NativeInitCtx::new(&transport, &mut throwaway_handles, Arc::clone(&self.mailer));
            // ADR-0081: wrap `init` in `with_stamped` so any
            // `tracing::*` event the actor fires lands in its
            // per-actor `ActorLogRing`. The pre-ADR
            // `with_actor_dispatch` + `drain_buffer` flush hop
            // retired alongside `LogBatch`.
            let init_result = local::with_stamped(&slots, || A::init(config, params, &mut init_ctx));
            match init_result {
                Ok(a) => a,
                Err(e) => return Err(SpawnError::InitFailed(e)),
            }
        };

        Ok(StagedActor {
            identity: SpawnIdentity { id, carry, canonical_name, subname },
            sender: tx,
            transport,
            slots,
            state,
            after_init,
        })
    }

    /// Convert an initialized actor into a storage-erased owner commit.
    ///
    /// `finalizer` is what decides the birth once the owner has ruled on it —
    /// every real birth carries one, differing only in where it delivers the
    /// [`SpawnOutcome`] (a parent actor's `TaskDone`, or the channel a
    /// post-seal external caller is blocked on). `None` is for fixtures that
    /// exercise the owner path with nothing waiting on the answer.
    ///
    /// `chain` is the staging site's ADR-0168 §3 declaration of what orders
    /// this birth's effects. It rides to the activation home so the newborn's
    /// `wire` hook can hold whatever chain it names (ADR-0168 §1).
    fn prepare_commit<A>(
        self: &Arc<Self>,
        staged: StagedActor<A>,
        finalizer: Option<Arc<super::activation::NativeSpawnFinalizer>>,
        chain: EffectChain,
    ) -> PreparedSpawnCommit
    where
        A: Instanced + NativeActor,
    {
        let StagedActor { identity, sender, transport, slots, state, after_init } = staged;
        let SpawnIdentity { id, canonical_name, subname, .. } = identity;
        // The actor's own declared kinds are seeded on top of whatever its
        // `init` already staged, never instead of it (iamacoffeepot/aether#4269).
        // Most actors stage nothing, so this is the plain "seed from the
        // declaration" it has always been. `WasmTrampoline` is the exception:
        // it pre-seeds the *guest's* handler set from `init`, and while that
        // pre-seed short-circuited the declaration, the trampoline's own
        // framework arms — `ReplaceComponent`, the ADR-0093 completion wake —
        // owned no cell, so every loaded component ran them unmeasured.
        // Merging keeps a staged kind's exact cell (the guest's cells are
        // already stamped into the per-actor cache) and mints one only for a
        // declared kind that has none.
        let mut costs = local::with_stamped(&slots, || {
            use aether_actor::Local as _;
            CostCells::with(|cells| cells.entries().to_vec())
        });
        let staged_kinds: HashSet<KindId> = costs.iter().map(|(kind, _)| *kind).collect();
        let missing: Vec<_> = A::measured_kinds()
            .into_iter()
            .filter(|kind| !staged_kinds.contains(kind))
            .map(|kind| (kind, Arc::new(CostCell::new())))
            .collect();
        if !missing.is_empty() {
            costs.extend(missing);
            local::with_stamped(&slots, || {
                use aether_actor::Local as _;
                CostCells::with_mut(|cells| cells.seed(costs.clone()));
            });
        }
        let after_init = after_init
            .into_iter()
            .map(|envelope| {
                PreparedMail::bootstrap(
                    Mail::new(id, envelope.kind, envelope.payload, envelope.count)
                        .with_reply_to(envelope.sender)
                        .with_lineage(envelope.mail_id, envelope.root, envelope.parent_mail),
                    envelope.kind_name,
                )
            })
            .collect();
        let activation = super::activation::LegacyPreparedActivation::<A>::new(
            Arc::clone(self),
            id,
            subname,
            sender,
            transport,
            slots,
            state,
            chain,
        );
        let activation = match finalizer {
            Some(finalizer) => activation.with_finalizer(finalizer),
            None => activation,
        };
        PreparedSpawnCommit::new(
            PreparedRoute::with_id(id, canonical_name.to_string()),
            Box::new(activation),
            PreparedCostCells::new(Arc::clone(self.mailer.cost_table()), costs),
            after_init,
        )
    }

    /// Consume the prepared birth, taking whichever of the two commit routes
    /// the ADR-0165 seal leaves open.
    ///
    /// Before the seal the boot path still holds this `Spawner`'s
    /// [`BootAuthority`], so the birth lands through the direct writer with
    /// read-your-writes and a typed error (#4035's carve-out). After it, no
    /// token exists and the only reachable route is the registry owner, which
    /// gives a root birth the same `Starting` / `wire`-at-home / `Live`
    /// protocol every other birth already runs.
    ///
    /// The authority guard is held across the direct branch rather than
    /// re-locked per mutator: nothing the branch runs re-enters `commit` (a
    /// handler's `spawn_child` stages, it never commits), and the only other
    /// contender for this lock is [`Self::seal`], which runs once boot is
    /// over.
    fn commit<A>(self: Arc<Self>, staged: StagedActor<A>) -> Result<SpawnCommit, SpawnError>
    where
        A: Instanced + NativeActor,
    {
        let authority = self.authority.lock().expect("spawner boot authority lock poisoned; fail-fast per ADR-0063");
        if let Some(authority) = authority.as_ref() {
            return Self::commit_directly(&self, staged, authority);
        }
        drop(authority);
        self.commit_through_owner(staged)
    }

    /// Submit the prepared birth to the ADR-0165 registry owner and block on
    /// its decision.
    ///
    /// Blocking is safe precisely where this runs: the caller is an external
    /// embedder thread reaching in through `BuiltChassis::spawn_actor` /
    /// `PassiveChassis::spawn_actor`, never a pool worker, so it cannot be the
    /// worker the owner needs to make progress. ADR-0165's one-worker-deadlock
    /// warning is about a *handler* waiting on the owner; a handler reaches
    /// `HandlerSpawnBuilder::stage` instead, which never waits.
    ///
    /// Both legs of the birth are what the caller is told about: the owner
    /// reserves the route `Starting` under a token, then the activation runs at
    /// its execution home, where `wire` runs and the barrier that promotes it
    /// to `Live` originates. The birth's finalizer decides it at the end of
    /// that second leg — after the owner has published the Live route — and
    /// delivers the [`SpawnOutcome`] down a channel this thread parks on, so
    /// `finish()` keeps the read-your-writes contract its callers have always
    /// had: when it returns `Ok`, the mailbox is addressable.
    ///
    /// The owner's batch completion is deliberately dropped rather than
    /// awaited. Every owner-side refusal of a `PreparedSpawn` — the pre-reserve
    /// route conflict, a rejected `reserve`, a refused cost row, and owner
    /// closure — routes that same commit through its finalizer, so the birth
    /// completion is the single answer, and it is the more precise one: it
    /// distinguishes a retired name from a live occupant where the batch error
    /// collapses both into a name conflict.
    fn commit_through_owner<A>(self: Arc<Self>, staged: StagedActor<A>) -> Result<SpawnCommit, SpawnError>
    where
        A: Instanced + NativeActor,
    {
        let mailbox_id = staged.identity.id;
        let (decided, birth) = crossbeam_channel::bounded(1);
        let finalizer = super::activation::NativeSpawnFinalizer::external(
            decided,
            mailbox_id,
            Arc::clone(&staged.identity.canonical_name),
            Arc::clone(&self.mailer),
        );
        let commit = self.prepare_commit(staged, Some(finalizer), EffectChain::Uncaused(Uncaused::EmbedderCall));
        if self.registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).is_none() {
            return Err(SpawnError::OwnerClosed);
        }
        match birth.recv_timeout(BIRTH_PATIENCE) {
            Ok(SpawnOutcome { mailbox_id, canonical_name, result }) => {
                result.map(|()| SpawnCommit { mailbox_id, canonical_name: canonical_name.to_string() })
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    target: "aether_substrate::spawn",
                    mailbox = %mailbox_id,
                    cap_millis = BIRTH_PATIENCE.as_millis(),
                    "post-seal spawn wedged: the owner accepted the birth but nothing decided it",
                );
                Err(SpawnError::BirthWedged { mailbox_id, waited: BIRTH_PATIENCE })
            }
            // The finalizer dropped without deciding, so no answer is coming.
            // A birth abandoned before anything promoted it leaves the caller
            // exactly where a refused activation does.
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(SpawnError::ActivationRejected),
        }
    }

    /// The pre-seal direct commit: every shared write and lifecycle action in
    /// the established order, on the calling thread.
    #[allow(clippy::too_many_lines)]
    fn commit_directly<A>(
        self: &Arc<Self>,
        staged: StagedActor<A>,
        authority: &BootAuthority,
    ) -> Result<SpawnCommit, SpawnError>
    where
        A: Instanced + NativeActor,
    {
        let StagedActor { identity, sender: tx, transport, slots, state, after_init } = staged;
        let SpawnIdentity { id, canonical_name: full_name, subname, .. } = identity;

        // Register sink + Live entry + pre-load mail. The actor
        // registry's `insert_live` and the mailbox registry's
        // `try_register_inbox` each take their own write lock; a
        // collision on either step rolls back. Sequence chosen so the
        // sink is the gating step (its `try_register_inbox` is the
        // only op that can fail with a name collision against a peer
        // singleton claim — the actor_registry slot is keyed on
        // MailboxId which already passed the tombstone check).
        //
        // The strong `Arc<Sender>` lives in the actor_registry's
        // Live entry. The sink handler's `Weak<Sender>` upgrades only
        // while the Arc is alive — i.e. while the actor's slot is
        // Live. On `mark_dead` the Arc drops, the weak upgrade fails,
        // and external mail addressed to the dead mailbox warn-drops.
        let strong_sender: Arc<mpsc::Sender<Envelope>> = Arc::new(tx.clone());
        let weak_for_handler = Arc::downgrade(&strong_sender);
        // Issue 635 PR C: pool wake hook. Populated post-init below
        // (every actor is pool-dispatched since issue 1187); empty until
        // then so the closure's `get()` is a single relaxed atomic load.
        let wake_slot: Arc<MailboxWakeSlot> = Arc::new(MailboxWakeSlot::default());
        let wake_for_handler = Arc::clone(&wake_slot);
        // iamacoffeepot/aether#848 PR 3: closure takes `OwnedDispatch`
        // and routes it through [`relay_or_transfer`] — the shared
        // upgrade → send → wake core with both ADR-0094 transfer seams.
        // ADR-0099 §3: register under the lineage-folded `id`, not
        // `hash(full_name)` — the rendered name is display / reverse-map
        // only and no longer derives the id.
        let registered = self.registry.try_register_inbox_with_id(
            authority,
            id,
            full_name.to_string(),
            Arc::new(move |dispatch: OwnedDispatch| {
                match relay_or_transfer(dispatch, &weak_for_handler, &wake_for_handler) {
                    RelayOutcome::Delivered => {}
                    RelayOutcome::SenderGone { kind_name } => {
                        tracing::warn!(
                            target: "aether_substrate::spawn",
                            kind = %kind_name,
                            "instanced actor sender dropped — mail discarded"
                        );
                    }
                    RelayOutcome::ReceiverGone { kind_name } => {
                        tracing::warn!(
                            target: "aether_substrate::spawn",
                            kind = %kind_name,
                            "instanced actor receiver dropped — mail discarded"
                        );
                    }
                }
            }),
        );
        match registered {
            Ok(returned_id) => debug_assert_eq!(returned_id, id),
            Err(NameConflict { name }) => return Err(SpawnError::SubnameInUse { full_name: name }),
        }

        // Issue 629 / Phase A: dispatcher takes Box<A> ownership.
        // The chassis-side actor_registry no longer holds a clone of
        // the actor — only the sender + type_id + subname for routing
        // and resolve_actor.
        let mut actor = Box::new(state);

        // Insert before pre-loading mail: the actor_registry holding
        // the sender is the canonical record that the slot is live.
        // The Arc<Sender> here is the same one the sink handler's
        // Weak references — when `mark_dead` drops this entry, the
        // weak upgrade fails for any further external mail.
        if self.actor_registry.insert_live(id, Arc::clone(&strong_sender), TypeId::of::<A>(), subname).is_err() {
            // Hash collision against an existing Live entry on the
            // same id but a slot the mailbox registry didn't reject —
            // possible if a singleton + instanced collide on the same
            // 64-bit id even with distinct names. Treat as
            // SubnameInUse for the caller; the singleton's claim wins
            // (it landed first).
            //
            // Issue 607 Phase 7: the sink WAS registered above; remove
            // it before returning so the failed spawn doesn't leave
            // a dangling sink that warn-drops mail. The actor itself
            // (init succeeded) drops naturally as `actor` falls out
            // of scope.
            self.registry.remove_closure(authority, id);
            return Err(SpawnError::SubnameInUse { full_name: full_name.to_string() });
        }

        // iamacoffeepot/aether#3051: seed every declared handler into
        // the shared cost table and stamp those exact cells into this
        // spawned actor's local cache. An actor whose init already installed
        // a dynamic handler set (notably WasmTrampoline's guest manifest)
        // keeps that more specific cache instead of being overwritten by the
        // wrapper actor's static capabilities.
        let actor_local_costs = local::with_stamped(&slots, || {
            use aether_actor::Local as _;
            CostCells::with(|cells| cells.entries().to_vec())
        });
        if actor_local_costs.is_empty() {
            let handler_kinds: Vec<KindId> = A::measured_kinds();
            let seeded = self.mailer.cost_table().seed(id, &handler_kinds);
            local::with_stamped(&slots, || {
                use aether_actor::Local as _;
                CostCells::with_mut(|cells| cells.seed(seeded));
            });
        } else {
            assert!(
                self.mailer.cost_table().install_live(id, &actor_local_costs),
                "new eager actor must own vacant cost rows"
            );
        }

        // Issue 584 Phase 2a (ADR-0079 amended): post-init mail-allowed
        // hook. Sink + actor_registry insert_live above means the
        // mailbox is fully published; peers are addressable and any
        // wire-time self-mail lands in this binding's inbox before the
        // dispatcher pulls. Runtime-spawn doesn't need the chassis-boot
        // multi-pass barrier (issue 697) because the substrate is
        // already steady-state when `Spawner::spawn_actor` runs — the
        // child wire→dispatcher transition is sequential within this
        // ctx, peers are running, all mailboxes claimed.
        //
        // This is the pre-seal direct route, reachable only while the boot
        // authority is unspent, so its caller is boot itself.
        local::with_stamped(&slots, || {
            let mut wire_ctx = NativeCtx::for_wire(&transport, EffectChain::Uncaused(Uncaused::ChassisBoot));
            A::wire(actor.as_mut(), &mut wire_ctx);
        });

        // Pre-load bootstrap mail. tx is alive (rx is held by the
        // transport; nobody's polling yet), so these sends always
        // succeed.
        for env in after_init {
            // mpsc::Sender::send only fails when the receiver
            // disconnects; rx is alive here. Discard on the
            // theoretical impossibility.
            let _ = tx.send(env);
        }

        // 8. Pool-register the dispatcher (every actor is pool-dispatched
        // since issue 1187 removed the per-thread `Dedicated` opt-out).
        // The local strong Arc was the populator for the Weak handler
        // ref; the actor_registry now holds an `Arc::clone` of the
        // same Arc, so dropping the local doesn't break the weak.
        drop(strong_sender);
        // Issue 635 PR C + Phase 3: register a `DispatcherSlot` with the
        // chassis worker pool. No per-actor thread. The wake hook on the
        // closure pushes the slot to the ready queue when an envelope
        // lands.
        let slot = DispatcherSlot::<A>::new(
            actor,
            Arc::clone(&transport),
            slots,
            Arc::clone(&self.actor_registry),
            Arc::clone(&self.mailer),
            id,
        );
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let weak: Weak<dyn Drainable> = Arc::downgrade(&slot_dyn);
        // iamacoffeepot/aether#1135: surface the seize handle on this
        // instanced actor's `Inbox` entry so the blob demuxer dispatches
        // its fan-out in place (ADR-0087 §4). The registry holds the
        // strong slot ref via `instanced_slots` below; the demuxer's
        // `Weak` upgrade fails cleanly once the actor is torn down.
        self.registry.install_seize_handle(
            authority,
            id,
            SeizeHandle::new(Arc::clone(slot.state()), Arc::downgrade(&slot_dyn)),
        );
        let wake = WakeHandle::new(Arc::clone(slot.state()), weak, self.wake_sink.clone());
        // Stash the slot's strong Arc so wakes can upgrade their `Weak`.
        // PR C dropped it here, which broke every wake after spawn (the
        // registry only holds the inbox sender, not the slot — the
        // comment claiming otherwise was wrong). Slots live until the
        // Spawner itself drops at chassis teardown. Issue 685 also
        // stashes a wake clone so chassis teardown can fire one wake per
        // slot after signaling shutdown.
        drop(slot);
        let teardown_wake = wake.clone();
        self.instanced_slots
            .lock()
            .expect("instanced_slots mutex poisoned; fail-fast per ADR-0063")
            .insert(id, InstancedSlotEntry { slot: slot_dyn, wake: teardown_wake });
        // Pre-loaded `after_init` mail (lines above) was sent straight to
        // the inbox via `tx.send`, which bypasses the closure's wake
        // hook. Fire one wake now so the slot enters the ready queue and
        // the worker drains those envelopes; subsequent peer sends route
        // through the closure and wake on their own.
        let manual_wake = wake.clone();
        wake_slot.set(Arc::new(move || {
            // Inbox-sender hook: the CAS-win bool would tell us whether
            // *this* sender owns the schedule push, but the scheduler
            // self-deduplicates so either outcome is fine.
            let _ = wake.wake();
        }));
        // Manual catch-up wake for inbox mail that landed before the
        // closure was installed (see comment above).
        let _ = manual_wake.wake();

        Ok(SpawnCommit { mailbox_id: id, canonical_name: full_name.to_string() })
    }
}

/// Eager builder for the boot/embedder boundary, returned from
/// `BuiltChassis::spawn_actor` and `PassiveChassis::spawn_actor` (and the
/// `cfg`-gated `spawn_actor_for_test`), and wrapped by
/// [`HandlerSpawnBuilder`] for handler-local child staging. Lets the caller
/// chain `after_init` to pre-load bootstrap mail before its terminal
/// operation.
///
/// Holds the spawner reference borrowed from the calling ctx's
/// transport, the resolved subname, the consumed config, and the
/// running list of after-init envelopes. `finish` consumes the
/// builder and runs the spawn lifecycle.
///
/// Its [`finish`](Self::finish) / [`finish_with_name`](Self::finish_with_name)
/// terminals write the registry, liveness, cost, and slot state synchronously,
/// which is correct only before the ADR-0165 owner seal. Both constructors are
/// crate-internal, so the only builders that reach outside the substrate come
/// from those chassis entry points; handler code holds a
/// [`HandlerSpawnBuilder`] instead and can reach nothing but the staged
/// terminals.
pub struct SpawnBuilder<'ctx, A: Instanced + NativeActor> {
    spawner: Arc<Spawner>,
    subname: Subname<'ctx>,
    config: Option<A::Config>,
    /// ADR-0156 §2 composer-supplied params, threaded to `A::init` beside
    /// `config`. Taken with `config` when `finish` runs.
    params: Option<A::Params>,
    sender: Source,
    /// ADR-0165: the spawning actor's typed runtime identity, or
    /// `None` for a top-level chassis-level spawn. `Some` nests the
    /// child — its id folds the new node's `ActorId` onto the parent
    /// carry, and its registered name renders under the parent's. `None`
    /// is the depth-1 case: the child is the root of its own lineage and
    /// keeps the flat `{NAMESPACE}:{subname}` id it has today.
    parent: Option<ActorRuntimeIdentity>,
    after_init: Vec<Envelope>,
    _marker: PhantomData<fn() -> A>,
    /// Carries the `'ctx` lifetime even though `spawner` is `Arc`
    /// (no longer borrowed). The lifetime ties `Subname::Named(&str)`
    /// to whatever borrow it was constructed from at the call site,
    /// so a stack-local subname doesn't dangle past `finish()`.
    _ctx: PhantomData<&'ctx ()>,
}

/// Handler-owned child builder, the only spawn surface
/// [`NativeCtx::spawn_child`](crate::actor::native::ctx::NativeCtx::spawn_child) hands back.
/// Every terminal it carries — [`stage`](Self::stage),
/// [`stage_with`](Self::stage_with), [`continue_from`](Self::continue_from) —
/// performs only local preparation during the actor turn and appends one
/// ordered prepared birth to the parent binding, so a handler never takes the
/// spawn path's shared locks mid-turn (ADR-0165). It deliberately exposes no
/// eager terminal: the wrapped [`SpawnBuilder`] is private, so reaching
/// synchronous commit from a handler needs an explicit substrate API change,
/// not a call-site choice.
pub struct HandlerSpawnBuilder<'ctx, A: Instanced + NativeActor> {
    inner: SpawnBuilder<'ctx, A>,
    parent_binding: Arc<NativeBinding>,
    completion_root: MailId,
    completion_reply_to: Source,
    /// ADR-0168 §3: what orders this birth's effects. Defaults to the
    /// calling handler's chain, which is the answer for every staging site
    /// that runs on a dispatched mail turn; [`Self::ordered_by`] replaces it
    /// where a device other than a hold does the ordering.
    chain: EffectChain,
}

impl<'ctx, A: Instanced + NativeActor> HandlerSpawnBuilder<'ctx, A> {
    pub(crate) fn new(
        inner: SpawnBuilder<'ctx, A>,
        parent_binding: Arc<NativeBinding>,
        completion_root: MailId,
        completion_reply_to: Source,
    ) -> Self {
        Self { inner, parent_binding, completion_root, completion_reply_to, chain: EffectChain::Held(completion_root) }
    }

    /// Declare that a device other than a settlement hold orders this birth
    /// (ADR-0168 §3).
    ///
    /// Reach for it at a staging site whose context carries no chain — a
    /// `PumpedSlot::host_turn`, a native-callback turn — where the ordering
    /// is real but comes from somewhere the staging call cannot show. A bare
    /// [`stage`](Self::stage) there takes no hold and says nothing about why
    /// that is correct, which is the reading #4199's inventory had to resolve
    /// by hand.
    ///
    /// Changes nothing about what the birth does: a chainless context yields
    /// no hold either way. The declaration is the point.
    #[must_use]
    pub fn ordered_by(mut self, device: OrderingDevice) -> Self {
        self.chain = EffectChain::OrderedBy(device);
        self
    }

    #[allow(clippy::needless_pass_by_value)]
    #[must_use]
    pub fn after_init<K>(mut self, mail: K) -> Self
    where
        A: HandlesKind<K>,
        K: Kind,
    {
        self.inner = self.inner.after_init(mail);
        self
    }

    /// Prepare and stage a birth whose later typed completion carries unit
    /// context.
    pub fn stage(self) -> Result<SpawnReceipt, SpawnError> {
        self.stage_with(())
    }

    /// Prepare and stage a birth with caller-owned completion context. The
    /// authoritative result later lands as `TaskDone<SpawnOutcome, C>`.
    ///
    /// # Panics
    ///
    /// Panics only if internal builder state has already been consumed, which
    /// safe code cannot do because this method takes ownership of `self`.
    pub fn stage_with<C>(self, context: C) -> Result<SpawnReceipt, SpawnError>
    where
        C: Send + 'static,
    {
        let Self { inner, parent_binding, completion_root, completion_reply_to, chain } = self;
        let SpawnBuilder { spawner, subname, config, params, sender, parent, after_init, .. } = inner;
        let config = config.expect("HandlerSpawnBuilder::stage consumed exactly once");
        let params = params.expect("HandlerSpawnBuilder::stage consumed exactly once");
        if let Subname::Named(subname) = subname {
            validate_namespace_segment(subname).map_err(SpawnError::SubnameInvalid)?;
        }
        let parent = parent.expect("handler child builder always carries a typed parent identity");

        let identity = spawner.prepare_identity::<A>(subname, Some(&parent))?;
        let key = ChildReservationKey::new(
            ActorId::singleton(A::NAMESPACE),
            ActorId::instanced(A::NAMESPACE, &identity.subname),
        );
        let parent_reservation = parent_binding
            .reserve_child(key)
            .ok_or_else(|| SpawnError::SubnameInUse { full_name: identity.canonical_name.to_string() })?;
        let staged = spawner.build::<A>(identity, config, params, after_init)?;
        let completion = parent_binding.dispatch_arm(
            spawner.mailer().acquire_settlement_hold(completion_root),
            completion_reply_to,
            context,
        );
        let receipt = SpawnReceipt {
            mailbox_id: staged.identity.id,
            canonical_name: Arc::clone(&staged.identity.canonical_name),
            completion: completion.dispatch_id(),
        };
        let finalizer = super::activation::NativeSpawnFinalizer::parented(
            parent_reservation,
            completion,
            staged.identity.id,
            Arc::clone(&staged.identity.canonical_name),
            Arc::downgrade(&staged.transport),
            Arc::clone(spawner.mailer()),
        );
        let commit = spawner.prepare_commit(staged, Some(finalizer), chain);
        parent_binding.stage_child_birth(commit);
        let _ = sender;
        Ok(receipt)
    }

    /// Stage a successor birth that inherits an already-owed reply — either a
    /// bare [`DeferredReply`](crate::actor::native::DeferredReply) or the
    /// [`TaskDone`](crate::actor::native::TaskDone) a completion handler is already holding,
    /// whose debt rides inside it. Every synchronous validation/build failure
    /// hands `owed` back untouched so the original terminal reply can still be
    /// sent exactly once.
    ///
    /// # Panics
    ///
    /// Panics only if internal builder state has already been consumed, which
    /// safe code cannot do because this method takes ownership of `self`.
    #[allow(
        clippy::result_large_err,
        reason = "the cold synchronous rejection returns the move-only owed reply intact beside the precise SpawnError"
    )]
    pub fn continue_from<R, C>(self, owed: R, context: C) -> Result<SpawnReceipt, (SpawnError, R)>
    where
        R: IntoDeferredReply,
        C: Send + 'static,
    {
        let Self { inner, parent_binding, .. } = self;
        let SpawnBuilder { spawner, subname, config, params, sender, parent, after_init, .. } = inner;
        let config = config.expect("HandlerSpawnBuilder::continue_from consumed exactly once");
        let params = params.expect("HandlerSpawnBuilder::continue_from consumed exactly once");
        if let Subname::Named(subname) = subname
            && let Err(error) = validate_namespace_segment(subname).map_err(SpawnError::SubnameInvalid)
        {
            return Err((error, owed));
        }
        let parent = parent.expect("handler child builder always carries a typed parent identity");

        let identity = match spawner.prepare_identity::<A>(subname, Some(&parent)) {
            Ok(identity) => identity,
            Err(error) => return Err((error, owed)),
        };
        let key = ChildReservationKey::new(
            ActorId::singleton(A::NAMESPACE),
            ActorId::instanced(A::NAMESPACE, &identity.subname),
        );
        let Some(parent_reservation) = parent_binding.reserve_child(key) else {
            return Err((SpawnError::SubnameInUse { full_name: identity.canonical_name.to_string() }, owed));
        };
        let staged = match spawner.build::<A>(identity, config, params, after_init) {
            Ok(staged) => staged,
            Err(error) => return Err((error, owed)),
        };
        // Every fallible step is behind us, so the debt can finally leave the
        // caller's hands: converting is what makes returning it impossible.
        let (hold, reply_to) = owed.into_deferred_reply().into_parts();
        // The inherited debt names the chain that caused this birth — the
        // successor's own ctx no longer holds it, and the newborn's `wire`
        // hook needs it to cover a birth-completing effect (ADR-0168 §1).
        let chain = EffectChain::Held(hold.as_ref().map_or(MailId::NONE, SettlementHold::root));
        let completion = parent_binding.dispatch_arm(hold, reply_to, context);
        let receipt = SpawnReceipt {
            mailbox_id: staged.identity.id,
            canonical_name: Arc::clone(&staged.identity.canonical_name),
            completion: completion.dispatch_id(),
        };
        let finalizer = super::activation::NativeSpawnFinalizer::parented(
            parent_reservation,
            completion,
            staged.identity.id,
            Arc::clone(&staged.identity.canonical_name),
            Arc::downgrade(&staged.transport),
            Arc::clone(spawner.mailer()),
        );
        parent_binding.stage_child_birth(spawner.prepare_commit(staged, Some(finalizer), chain));
        let _ = sender;
        Ok(receipt)
    }
}

impl<'ctx, A: Instanced + NativeActor> SpawnBuilder<'ctx, A> {
    /// Internal constructor for the top-level chassis spawn. Public only
    /// because chassis-level `spawn_actor` entry points (on `BuiltChassis` /
    /// `PassiveChassis`) build these too.
    ///
    /// It takes no parent: this is the depth-1 placement, so the child is the
    /// root of its own lineage and keeps the flat `{NAMESPACE}:{subname}` id
    /// (ADR-0099 §3). A birth under a parent goes through
    /// [`Self::new_child`] instead (issue 4135).
    pub(crate) fn new(
        spawner: Arc<Spawner>,
        subname: Subname<'ctx>,
        config: A::Config,
        params: A::Params,
        sender: Source,
    ) -> Self {
        Self {
            spawner,
            subname,
            config: Some(config),
            params: Some(params),
            sender,
            parent: None,
            after_init: Vec::new(),
            _marker: PhantomData,
            _ctx: PhantomData,
        }
    }

    /// Internal constructor for a birth under a parent actor.
    ///
    /// `parent` is the spawning actor's own runtime identity, read off the
    /// staging ctx — never a caller-declared type — so there is nothing here
    /// to disagree with the executing binding (issue 4158).
    pub(crate) fn new_child(
        spawner: Arc<Spawner>,
        subname: Subname<'ctx>,
        config: A::Config,
        params: A::Params,
        sender: Source,
        parent: ActorRuntimeIdentity,
    ) -> Self {
        Self {
            spawner,
            subname,
            config: Some(config),
            params: Some(params),
            sender,
            parent: Some(parent),
            after_init: Vec::new(),
            _marker: PhantomData,
            _ctx: PhantomData,
        }
    }

    /// Append `mail` to the bootstrap sequence. Order-preserving —
    /// the spawned actor sees envelopes in the order they were added.
    /// Sender on each envelope is the spawner's reply target; `reply_to`
    /// defaults to the spawner's mailbox.
    ///
    /// `A: HandlesKind<K>` ensures only kinds the actor's handler set
    /// covers can be pre-loaded; the strict-receiver miss path stays
    /// off the bootstrap surface.
    // `mail` is taken by value so the builder API mirrors the rest of
    // the spawn surface (`config: A::Config` is also by value); the
    // value flows straight into `encode_into_bytes` whose owned form
    // matches `Kind`'s wire-encoding convention.
    #[allow(clippy::needless_pass_by_value)]
    #[must_use]
    pub fn after_init<K>(mut self, mail: K) -> Self
    where
        A: HandlesKind<K>,
        K: Kind,
    {
        let payload = mail.encode_into_bytes();
        let kind = KindId(<K as Kind>::ID.0);
        // ADR-0094: the bootstrap seed carries no settlement lineage
        // (`MailId::NONE`), so it is built *disarmed* — there is no
        // obligation to discharge (and `dispatch_one` no-ops its
        // `record_finished` on `NONE` anyway).
        let env = Envelope::disarmed(
            kind,
            self.spawner.registry().kind_name_shared(kind).unwrap_or_else(|| Arc::from(K::NAME)),
            None,
            self.sender,
            MailRef::from(payload),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            // Bootstrap seed carries no lineage (`MailId::NONE`), so it
            // never folds into a traced tree node — no deposit instant to
            // record (iamacoffeepot/aether#1134).
            Nanos(0),
            0,
            MailboxId(0),
        );
        self.after_init.push(env);
        self
    }

    /// Consume the builder through the one shared terminal path.
    fn finish_internal(self) -> Result<SpawnCommit, SpawnError> {
        let SpawnBuilder { spawner, subname, config, params, sender, parent, after_init, .. } = self;
        let config = config.expect("SpawnBuilder::finish consumed exactly once");
        let params = params.expect("SpawnBuilder::finish consumed exactly once");
        if let Subname::Named(subname) = subname {
            validate_namespace_segment(subname).map_err(SpawnError::SubnameInvalid)?;
        }
        let _ = sender;
        let identity = spawner.preflight::<A>(subname, parent.as_ref())?;
        let staged = spawner.build::<A>(identity, config, params, after_init)?;
        spawner.commit(staged)
    }

    /// Consume the builder and run the spawn lifecycle. Returns the
    /// new actor's [`MailboxId`] on success, or a typed [`SpawnError`]
    /// describing which lifecycle step failed.
    ///
    /// Boot/embedder authority: the commit half writes shared registry and
    /// scheduler state on the calling thread, which the ADR-0165 owner seal
    /// permits only before the owner takes over. A handler stages instead —
    /// see [`HandlerSpawnBuilder::stage`].
    pub fn finish(self) -> Result<MailboxId, SpawnError> {
        self.finish_internal().map(|commit| commit.mailbox_id)
    }

    /// Consume the builder and return both the mailbox id and the exact
    /// canonical name registered for the new actor. Carries the same
    /// boot/embedder authority as [`Self::finish`].
    pub fn finish_with_name(self) -> Result<(MailboxId, String), SpawnError> {
        self.finish_internal().map(|commit| (commit.mailbox_id, commit.canonical_name))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "activation lifecycle tests use bounded channels and fixture-only setup")]

    use super::*;
    use aether_actor::Addressable;

    use crate::actor::native::{DispatchId, TaskDone};
    use crate::config::RegistryQueueCapacities;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::effect::{
        ActivationToken, EffectBatch, RegistryApplied, RegistryEffect, RegistryEffectError,
    };
    use crate::mail::registry::{MailDispatch, Registry, RegistryOwnerLease, RouteRelayLease, noop_handler};
    use crate::runtime::lifecycle::PanicAborter;
    use crate::scheduler::{BatchBudget, CycleResult, Pool, PoolConfig, PoolHandle, SlotState};
    use crate::testing::boot_authority;
    use std::any::Any;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    /// Probe methods whose names collide with the eager terminals. Rust resolves
    /// an inherent method ahead of a trait one, so these are reachable through
    /// method-call syntax on a [`HandlerSpawnBuilder`] exactly while that type
    /// declares no inherent `finish` / `finish_with_name` of its own.
    trait EagerTerminalProbe {
        fn finish(self) -> &'static str;
        fn finish_with_name(self) -> &'static str;
    }

    impl<A: Instanced + NativeActor> EagerTerminalProbe for HandlerSpawnBuilder<'_, A> {
        fn finish(self) -> &'static str {
            "probe"
        }

        fn finish_with_name(self) -> &'static str {
            "probe"
        }
    }

    /// The shape of a boot/embedder eager terminal, over whatever success value
    /// it hands back.
    type EagerTerminal<'ctx, A, R> = fn(SpawnBuilder<'ctx, A>) -> Result<R, SpawnError>;

    /// Tripwire (ADR-0165, iamacoffeepot/aether#4070): a handler builder has no
    /// eager terminal, and the boot/embedder builder still has one.
    ///
    /// Both bindings are compile-time assertions — the bodies never run, and
    /// the plausible bug is a future edit re-exposing synchronous commit to
    /// handler code. Re-adding `HandlerSpawnBuilder::finish` (or
    /// `finish_with_name`) makes the inherent method win method resolution
    /// above, so the `&'static str` bindings stop type-checking against
    /// `Result<MailboxId, SpawnError>` and this file fails to compile.
    /// Deleting the boot terminals breaks the paired coercions below, so the
    /// asymmetry is pinned from both sides rather than only one.
    #[allow(dead_code, reason = "the compile is the assertion; there is no handler binding to construct here")]
    fn spawn_terminals_stay_split<'ctx, A: Instanced + NativeActor>(
        staged_only: HandlerSpawnBuilder<'ctx, A>,
        staged_only_named: HandlerSpawnBuilder<'ctx, A>,
    ) {
        let _: &'static str = staged_only.finish();
        let _: &'static str = staged_only_named.finish_with_name();

        let _: EagerTerminal<'ctx, A, MailboxId> = SpawnBuilder::finish;
        let _: EagerTerminal<'ctx, A, (MailboxId, String)> = SpawnBuilder::finish_with_name;
    }

    #[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, aether_data::Kind, aether_data::Schema)]
    #[kind(name = "test.activation.poke")]
    struct ActivationPoke;

    /// Drives the probe down its ordinary self-close path — the handler flips
    /// the shutdown flag its dispatcher slot polls, exactly as a production
    /// actor that retires itself does.
    #[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, aether_data::Kind, aether_data::Schema)]
    #[kind(name = "test.activation.close")]
    struct ActivationClose;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ActivationEvent {
        Wire(thread::ThreadId),
        Dispatch(thread::ThreadId),
        Unwire(thread::ThreadId),
        Drop(thread::ThreadId),
    }

    struct ActivationProbe {
        events: crossbeam_channel::Sender<ActivationEvent>,
        lifecycle_target: Option<MailboxId>,
    }

    struct ActivationConfig {
        events: crossbeam_channel::Sender<ActivationEvent>,
        lifecycle_target: Option<MailboxId>,
    }

    impl ActivationConfig {
        fn new(events: crossbeam_channel::Sender<ActivationEvent>) -> Self {
            Self { events, lifecycle_target: None }
        }

        fn with_lifecycle_target(
            events: crossbeam_channel::Sender<ActivationEvent>,
            lifecycle_target: MailboxId,
        ) -> Self {
            Self { events, lifecycle_target: Some(lifecycle_target) }
        }
    }

    impl Drop for ActivationProbe {
        fn drop(&mut self) {
            let _ = self.events.send(ActivationEvent::Drop(thread::current().id()));
        }
    }

    #[aether_actor::actor(instanced, root)]
    impl NativeActor for ActivationProbe {
        const NAMESPACE: &'static str = "test.activation.probe";
        type Config = ActivationConfig;

        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { events: config.events, lifecycle_target: config.lifecycle_target })
        }

        fn wire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
            if let Some(target) = state.lifecycle_target {
                let _ = ctx.send_envelope_detached(target, ActivationPoke::ID, &ActivationPoke.encode_into_bytes());
            }
            let _ = state.events.send(ActivationEvent::Wire(thread::current().id()));
        }

        #[handler::single]
        fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _poke: ActivationPoke) {
            let _ = self.events.send(ActivationEvent::Dispatch(thread::current().id()));
        }

        #[handler::single]
        fn on_close(&mut self, ctx: &mut NativeCtx<'_>, _close: ActivationClose) {
            let _ = self.events.send(ActivationEvent::Dispatch(thread::current().id()));
            ctx.shutdown();
        }

        fn unwire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
            if let Some(target) = state.lifecycle_target {
                let _ = ctx.send_envelope_detached(target, ActivationPoke::ID, &ActivationPoke.encode_into_bytes());
            }
            let _ = state.events.send(ActivationEvent::Unwire(thread::current().id()));
        }
    }

    fn activation_fixture() -> (Arc<Spawner>, Arc<Registry>, Arc<Mailer>, PoolHandle) {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
        let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, Arc::clone(&aborter));
        let spawner = Arc::new(Spawner::new(
            Arc::clone(&registry),
            Arc::new(ActorRegistry::new()),
            Arc::clone(&mailer),
            aborter,
            pool.wake_sink(),
            RingCapacities::default(),
        ));
        (spawner, registry, mailer, pool)
    }

    fn prepared_probe(
        spawner: &Arc<Spawner>,
        name: &str,
        events: crossbeam_channel::Sender<ActivationEvent>,
    ) -> PreparedSpawnCommit {
        let identity = spawner.preflight::<ActivationProbe>(Subname::Named(name), None).unwrap();
        let staged = spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events), (), Vec::new()).unwrap();
        spawner.prepare_commit(staged, None, EffectChain::Uncaused(Uncaused::EmbedderCall))
    }

    fn prepared_probe_with_lifecycle_target(
        spawner: &Arc<Spawner>,
        name: &str,
        events: crossbeam_channel::Sender<ActivationEvent>,
        lifecycle_target: MailboxId,
    ) -> PreparedSpawnCommit {
        let identity = spawner.preflight::<ActivationProbe>(Subname::Named(name), None).unwrap();
        let staged = spawner
            .build::<ActivationProbe>(
                identity,
                ActivationConfig::with_lifecycle_target(events, lifecycle_target),
                (),
                Vec::new(),
            )
            .unwrap();
        spawner.prepare_commit(staged, None, EffectChain::Uncaused(Uncaused::EmbedderCall))
    }

    fn activation_sink(registry: &Registry, name: &str) -> (MailboxId, crossbeam_channel::Receiver<KindId>) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let id = registry.register_inline(
            &boot_authority(),
            name,
            Arc::new(move |dispatch: MailDispatch<'_>| {
                let _ = sender.send(dispatch.kind);
            }),
        );
        (id, receiver)
    }

    fn finalized_probe(
        spawner: &Arc<Spawner>,
        parent: &Arc<NativeBinding>,
        name: &str,
        events: crossbeam_channel::Sender<ActivationEvent>,
        correlation: u64,
    ) -> (PreparedSpawnCommit, DispatchId, ChildReservationKey) {
        let key = ChildReservationKey::new(
            ActorId::singleton(ActivationProbe::NAMESPACE),
            ActorId::instanced(ActivationProbe::NAMESPACE, name),
        );
        let parent_reservation = parent.reserve_child(key).expect("distinct staged parent key reservation wins");
        let identity = spawner.prepare_identity::<ActivationProbe>(Subname::Named(name), None).unwrap();
        let staged = spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events), (), Vec::new()).unwrap();
        let causing_chain = MailId::new(parent.self_mailbox(), correlation);
        let deferred = parent.dispatch_arm::<SpawnOutcome, _>(
            spawner.mailer().acquire_settlement_hold(causing_chain),
            Source::NONE,
            (),
        );
        let dispatch_id = deferred.dispatch_id();
        let finalizer = super::super::activation::NativeSpawnFinalizer::parented(
            parent_reservation,
            deferred,
            staged.identity.id,
            Arc::clone(&staged.identity.canonical_name),
            Arc::downgrade(&staged.transport),
            Arc::clone(spawner.mailer()),
        );

        (spawner.prepare_commit(staged, Some(finalizer), EffectChain::Held(causing_chain)), dispatch_id, key)
    }

    fn await_spawn_done(parent: &NativeBinding, dispatch_id: DispatchId) -> TaskDone<SpawnOutcome, ()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(done) = parent.dispatch_take(dispatch_id) {
                return done;
            }
            assert!(Instant::now() < deadline, "native finalizer filled its typed deferred result");
            thread::yield_now();
        }
    }

    #[test]
    fn prepared_bootstrap_mail_shares_the_registered_kind_name() {
        let (spawner, registry, _mailer, pool) = activation_fixture();
        registry
            .register_kind_with_descriptor(
                &boot_authority(),
                aether_data::KindDescriptor {
                    name: ActivationPoke::NAME.to_owned(),
                    schema: <ActivationPoke as aether_data::Schema>::SCHEMA,
                },
            )
            .unwrap();
        let registered = registry.kind_name_shared(ActivationPoke::ID).expect("registered kind has a shared name");
        let (events_tx, _events_rx) = crossbeam_channel::unbounded();
        let builder = SpawnBuilder::<ActivationProbe>::new(
            Arc::clone(&spawner),
            Subname::Named("shared-bootstrap-name"),
            ActivationConfig::new(events_tx),
            (),
            Source::NONE,
        )
        .after_init(ActivationPoke);

        assert_eq!(builder.after_init.len(), 1);
        assert!(
            Arc::ptr_eq(&builder.after_init[0].kind_name, &registered),
            "bootstrap preparation clones the registry-owned Arc"
        );

        assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
    }

    #[test]
    fn prepared_activation_lifecycle_stays_on_scheduler_home() {
        let (spawner, _registry, _mailer, pool) = activation_fixture();
        let caller = thread::current().id();

        let (discard_tx, discard_rx) = crossbeam_channel::unbounded();
        prepared_probe(&spawner, "discard", discard_tx).discard_at_home().recv_timeout(Duration::from_secs(1)).unwrap();
        let discarded = discard_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(discarded, ActivationEvent::Drop(home) if home != caller));
        assert!(discard_rx.try_recv().is_err(), "unwired lifecycle never ran for an unwired discard");

        let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded();
        let mut commit = prepared_probe(&spawner, "cancel", cancel_tx);
        let token = ActivationToken::from_value(1).unwrap();
        let activation = commit.take_activation().reserve(token).unwrap_or_else(|_| panic!("reservation accepted"));
        activation.schedule();
        let ActivationEvent::Wire(home) = cancel_rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
            panic!("wire runs first")
        };
        activation.cancel_and_join();
        assert_eq!(cancel_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Unwire(home));
        assert_eq!(cancel_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Drop(home));
        assert!(cancel_rx.try_recv().is_err(), "post-wire cancellation unwires exactly once");

        assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
    }

    #[test]
    fn owner_close_before_apply_rejects_native_finalizer_at_home_and_releases_parent_key() {
        let (spawner, registry, mailer, pool) = activation_fixture();
        let owner = RegistryOwnerLease::attach(
            boot_authority(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );
        let caller = thread::current().id();
        let parent_id = MailboxId::from_name("test.activation.owner-close-parent");
        let parent = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), parent_id));
        let key = ChildReservationKey::new(
            ActorId::singleton(ActivationProbe::NAMESPACE),
            ActorId::instanced(ActivationProbe::NAMESPACE, "owner-close-before-apply"),
        );
        let parent_reservation = parent.reserve_child(key).expect("first staged parent key reservation wins");
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let identity =
            spawner.prepare_identity::<ActivationProbe>(Subname::Named("owner-close-before-apply"), None).unwrap();
        let staged =
            spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events_tx), (), Vec::new()).unwrap();
        let causing_chain = MailId::new(parent_id, 1);
        let deferred =
            parent.dispatch_arm::<SpawnOutcome, _>(mailer.acquire_settlement_hold(causing_chain), Source::NONE, ());
        let dispatch_id = deferred.dispatch_id();
        let finalizer = super::super::activation::NativeSpawnFinalizer::parented(
            parent_reservation,
            deferred,
            staged.identity.id,
            Arc::clone(&staged.identity.canonical_name),
            Arc::downgrade(&staged.transport),
            Arc::clone(&mailer),
        );
        let commit = spawner.prepare_commit(staged, Some(finalizer), EffectChain::Held(causing_chain));
        let child_id = commit.route.id;
        let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();

        drop(owner);

        assert!(matches!(
            completion.wait_timeout(Duration::from_secs(1)).unwrap(),
            Err(RegistryEffectError::OwnerClosed)
        ));
        let dropped = events_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(dropped, ActivationEvent::Drop(home) if home != caller));
        assert!(events_rx.try_recv().is_err(), "pre-apply rejection drops without running unwire");
        assert!(registry.entry(child_id).is_none(), "owner-close rejection publishes no route");

        let done = parent
            .dispatch_take::<SpawnOutcome, ()>(dispatch_id)
            .expect("owner-close finalization fills the typed deferred result");
        assert_eq!(done.output().mailbox_id, child_id, "a rejection still names the birth it belongs to");
        assert!(matches!(done.output().result, Err(SpawnError::OwnerClosed)));
        done.release_no_reply();
        drop(parent.reserve_child(key).expect("owner-close rejection releases the staged parent key"));

        assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
    }

    #[test]
    fn rejected_multi_birth_batch_marks_unvisited_native_finalizer_as_activation_rejected() {
        let (spawner, registry, mailer, pool) = activation_fixture();
        let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
        let owner = RegistryOwnerLease::attach(
            boot_authority(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );
        let caller = thread::current().id();
        let parent = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId::from_name("test.activation.rejected-batch-parent"),
        ));
        let (first_tx, first_rx) = crossbeam_channel::unbounded();
        let (middle_tx, middle_rx) = crossbeam_channel::unbounded();
        let (later_tx, later_rx) = crossbeam_channel::unbounded();
        let (first, first_dispatch, first_key) = finalized_probe(&spawner, &parent, "batch-first", first_tx, 1);
        let (middle, middle_dispatch, middle_key) = finalized_probe(&spawner, &parent, "batch-middle", middle_tx, 2);
        let (later, later_dispatch, later_key) = finalized_probe(&spawner, &parent, "batch-later", later_tx, 3);
        let first_id = first.route.id;
        let middle_id = middle.route.id;
        let later_id = later.route.id;
        registry
            .try_register_inbox_with_id(
                &boot_authority(),
                middle_id,
                middle.route.canonical_name.clone(),
                noop_handler(),
            )
            .unwrap();
        let completion = registry
            .submit(EffectBatch::new(vec![
                RegistryEffect::PreparedSpawn(first),
                RegistryEffect::PreparedSpawn(middle),
                RegistryEffect::PreparedSpawn(later),
            ]))
            .unwrap();

        owner.run_once();

        assert!(matches!(completion.wait_timeout(Duration::from_secs(1)).unwrap(), Err(RegistryEffectError::Name(_))));
        for events in [&first_rx, &middle_rx, &later_rx] {
            let dropped = events.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(matches!(dropped, ActivationEvent::Drop(home) if home != caller));
            assert!(events.try_recv().is_err(), "rejected pre-wire state drops without unwire");
        }
        assert!(registry.entry(first_id).is_none());
        assert!(registry.entry(middle_id).is_some(), "the pre-existing middle conflict remains unchanged");
        assert!(registry.entry(later_id).is_none());

        let first_done = await_spawn_done(&parent, first_dispatch);
        assert_eq!(first_done.output().mailbox_id, first_id, "each rejection names its own birth");
        assert!(matches!(first_done.output().result, Err(SpawnError::ActivationRejected)));
        first_done.release_no_reply();
        let middle_done = await_spawn_done(&parent, middle_dispatch);
        assert_eq!(middle_done.output().mailbox_id, middle_id);
        assert!(matches!(middle_done.output().result, Err(SpawnError::SubnameInUse { .. })));
        middle_done.release_no_reply();
        let later_done = await_spawn_done(&parent, later_dispatch);
        assert_eq!(later_done.output().mailbox_id, later_id);
        assert!(matches!(later_done.output().result, Err(SpawnError::ActivationRejected)));
        later_done.release_no_reply();
        for key in [first_key, middle_key, later_key] {
            drop(parent.reserve_child(key).expect("transactional rejection releases every parent key"));
        }

        drop(owner);
        assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
    }

    #[test]
    fn successful_prepared_activation_enters_ordinary_dispatch_once() {
        let (spawner, registry, mailer, pool) = activation_fixture();
        let (lifecycle_target, lifecycle_mail) = activation_sink(&registry, "test.activation.live-effects");
        let owner = RegistryOwnerLease::attach(
            boot_authority(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let commit = prepared_probe_with_lifecycle_target(&spawner, "live", events_tx, lifecycle_target);
        let id = commit.route.id;
        let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
        owner.apply_once_then_observe_before_next_apply_for_test(|| {
            assert!(lifecycle_mail.try_recv().is_err(), "wire effects remain quarantined while the route is Starting");
            assert!(registry.entry(id).is_none(), "the owner has not yet promoted the Starting route");
        });
        let _ = completion.wait_timeout(Duration::from_secs(1)).unwrap().unwrap();
        let ActivationEvent::Wire(home) = events_rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
            panic!("wire runs before live dispatch")
        };
        assert!(registry.entry(id).is_some(), "barrier promotes the actor to Live");
        assert_eq!(
            lifecycle_mail.recv_timeout(Duration::from_secs(1)).unwrap(),
            ActivationPoke::ID,
            "the owner's post-publication suffix releases wire effects"
        );

        mailer.push(Mail::new(id, ActivationPoke::ID, ActivationPoke.encode_into_bytes(), 1));
        assert_eq!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Dispatch(home));
        assert!(events_rx.try_recv().is_err(), "one live mail performs one ordinary dispatcher drain");

        spawner.shutdown_instanced(Duration::from_millis(1), Duration::from_secs(1), &FatalAbortRecord::new());
        drop(owner);
        assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
    }

    /// Re-staging a self-closed child's subname reports the authoritative
    /// `SubnameRetired`, and the parent's key comes back at the child's own
    /// close path rather than at chassis teardown. Issue 4152's two
    /// independent regressions, in the order a caller meets them: the live
    /// key rode the child's binding inside [`Spawner::instanced_slots`],
    /// which only `shutdown_instanced` ever empties, so the re-stage was
    /// rejected locally as `SubnameInUse` and one table entry leaked per
    /// dead child; and the owner then rejected the birth on its surviving
    /// route — also as `SubnameInUse` — before
    /// [`super::activation::LegacyPreparedActivation::reserve`] could
    /// report the retirement. Either one alone turns the retired-name
    /// diagnostic (ADR-0165) into a "name in use" lie.
    #[test]
    fn closed_child_subname_restages_as_retired_not_in_use() {
        let (spawner, registry, mailer, pool) = activation_fixture();
        let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
        let owner = RegistryOwnerLease::attach(
            boot_authority(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );
        let parent = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId::from_name("test.activation.self-close-parent"),
        ));
        let (events_tx, _events_rx) = crossbeam_channel::unbounded();
        let (commit, dispatch_id, key) = finalized_probe(&spawner, &parent, "self-close", events_tx, 1);
        let child_id = commit.route.id;
        let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();

        owner.apply_once_then_observe_before_next_apply_for_test(|| {
            assert!(parent.reserve_child(key).is_none(), "the staged key stays held while the child is Starting");
        });
        completion.wait_timeout(Duration::from_secs(1)).unwrap().unwrap();

        let done = await_spawn_done(&parent, dispatch_id);
        assert!(matches!(done.output(), SpawnOutcome { mailbox_id, result: Ok(()), .. } if *mailbox_id == child_id));
        done.release_no_reply();
        assert!(parent.reserve_child(key).is_none(), "Live promotion carries the same key into the live-child set");

        mailer.push(Mail::new(child_id, ActivationClose::ID, ActivationClose.encode_into_bytes(), 1));

        let deadline = Instant::now() + Duration::from_secs(5);
        let restaged = loop {
            if let Some(restaged) = parent.reserve_child(key) {
                break restaged;
            }
            assert!(Instant::now() < deadline, "the closed child's close path released its parent-local key");
            thread::yield_now();
        };
        assert!(
            spawner.instanced_slots.lock().expect("instanced_slots mutex poisoned").contains_key(&child_id),
            "the key came back from the actor's close path — its slot is still parked for chassis teardown"
        );
        drop(restaged);

        let (events_tx, _events_rx) = crossbeam_channel::unbounded();
        let (reborn, reborn_dispatch, _) = finalized_probe(&spawner, &parent, "self-close", events_tx, 2);
        let rejection = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(reborn)])).unwrap();
        owner.run_once();

        assert!(matches!(rejection.wait_timeout(Duration::from_secs(1)).unwrap(), Err(RegistryEffectError::Name(_))));
        let reborn_done = await_spawn_done(&parent, reborn_dispatch);
        assert!(
            matches!(reborn_done.output().result, Err(SpawnError::SubnameRetired { .. })),
            "the owner classified the surviving route of a retired id, not a live occupant: {:?}",
            reborn_done.output()
        );
        reborn_done.release_no_reply();

        spawner.shutdown_instanced(Duration::from_millis(1), Duration::from_secs(1), &FatalAbortRecord::new());
        drop(owner);
        assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
    }

    #[test]
    fn owner_close_after_wire_cleans_starting_activation_at_home() {
        let (spawner, registry, mailer, pool) = activation_fixture();
        let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
        let (lifecycle_target, lifecycle_mail) = activation_sink(&registry, "test.activation.cancelled-effects");
        let owner = RegistryOwnerLease::attach(
            boot_authority(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let commit = prepared_probe_with_lifecycle_target(&spawner, "owner-close", events_tx, lifecycle_target);
        let id = commit.route.id;
        let canonical_name = commit.route.canonical_name.clone();
        let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
        owner.apply_once_then_close_after_next_command();
        let applied = completion.wait_timeout(Duration::from_secs(1)).unwrap().unwrap();
        let [RegistryApplied::Starting { token, .. }] = applied.as_slice() else {
            panic!("prepared birth publishes Starting")
        };
        let token = *token;
        let ActivationEvent::Wire(home) = events_rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
            panic!("activation wires before owner closure")
        };
        drop(owner);

        assert_eq!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Unwire(home));
        assert_eq!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Drop(home));
        assert!(events_rx.try_recv().is_err(), "owner closure unwires exactly once");
        assert!(
            lifecycle_mail.try_recv().is_err(),
            "neither wire nor rejection-time unwire effects escape a never-Live actor"
        );
        assert!(registry.lookup(&canonical_name).is_none(), "Starting route is rolled back without Live publication");
        assert!(registry.entry(id).is_none());
        assert!(mailer.cost_table().cells_for(id).is_empty(), "token-owned cost rows are rolled back");
        let fresh = ActivationToken::from_value(token.value() + 1).unwrap();
        assert!(spawner.actor_registry().reserve_starting(id, fresh), "actor lifecycle reservation was removed");
        spawner.actor_registry().rollback_starting(id, fresh);

        assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
    }

    /// A `Drainable` that never runs its close cycle: it stashes the
    /// close-done sender the teardown gate installs and holds it alive
    /// without ever firing it. The gate's per-slot waiter therefore stays
    /// connected but silent, so the wait exhausts its cumulative cap — the
    /// starvation-shaped wedge (a healthy-but-slow / stuck close cycle)
    /// #2509 guards, not an immediate channel disconnect.
    struct NeverClosingSlot {
        close_done: Mutex<Option<crossbeam_channel::Sender<()>>>,
    }

    impl Drainable for NeverClosingSlot {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }
        fn set_close_done_tx(&self, tx: crossbeam_channel::Sender<()>) {
            *self.close_done.lock().expect("close_done mutex never poisoned in this single-threaded test") = Some(tx);
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Tripwire (issue #2509): a wedged instanced-actor teardown gate
    /// names the slot that failed to close. The `expected` substring pins
    /// the gate label embedding the slot's `MailboxId` Display
    /// (`shutdown_instanced.close_done[<id>]`) — a real tagged mailbox id
    /// renders as `mbx-…`; this test's raw id falls back to the `{:#018x}`
    /// hex form. If the label ever drops the id and reverts to the bare
    /// `shutdown_instanced.close_done`, the substring stops matching and
    /// this test fails.
    ///
    /// Fast by construction: the cumulative cap is injected directly as
    /// `shutdown_instanced`'s parameter (20 ms), so the wedge fires in
    /// milliseconds rather than blocking on the 300 s default.
    #[test]
    #[should_panic(expected = "shutdown_instanced.close_done[0x000000000000abcd]")]
    fn shutdown_instanced_wedge_names_the_slot() {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
        let actor_registry = Arc::new(ActorRegistry::new());
        // One worker is enough — the wedge comes from the close-done
        // signal never firing, not from anything the pool drains.
        let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, Arc::clone(&aborter));
        let spawner = Spawner::new(
            Arc::clone(&registry),
            actor_registry,
            Arc::clone(&mailer),
            Arc::clone(&aborter),
            pool.wake_sink(),
            RingCapacities::default(),
        );

        let slot: Arc<dyn Drainable> = Arc::new(NeverClosingSlot { close_done: Mutex::new(None) });
        let wake = WakeHandle::new(Arc::new(SlotState::new()), Arc::downgrade(&slot), pool.wake_sink());
        spawner
            .instanced_slots
            .lock()
            .expect("instanced_slots mutex poisoned")
            .insert(MailboxId(0xABCD), InstancedSlotEntry { slot, wake });

        spawner.shutdown_instanced(Duration::from_millis(1), Duration::from_millis(20), &FatalAbortRecord::new());
    }
}
