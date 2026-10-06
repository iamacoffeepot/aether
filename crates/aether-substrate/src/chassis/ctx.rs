//! Boot machinery shared by the chassis builder (`chassis::builder::Builder`,
//! ADR-0071): the mailbox claim, its [`MailboxClaim`] result shape, and
//! the [`ChassisCtx`] threaded through every cap's boot. Sibling modules:
//! error types live in `chassis::error`; the cross-flavour [`Envelope`]
//! shape lives in `actor::native::envelope`; the inbox channel and the
//! relay a claim registers live in `chassis::inbox`.

use std::collections::HashMap;
use std::sync::Arc;

use aether_actor::local::ActorSlots;
use aether_actor::{ActorRef, Addressable};

use crate::actor::monitor::post_notices;
use crate::actor::native::envelope::Envelope;
use crate::chassis::builder::ComposedReferences;
use crate::chassis::error::BootError;
use crate::chassis::inbox::{SettlingInbox, inbox_channel};
use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::BootAuthority;
use crate::mail::registry::Registry;
use crate::runtime::lifecycle::FatalAborter;
use crate::scheduler::WakeSink;
use std::fmt;
use std::sync::OnceLock;

// iamacoffeepot/aether#848 PR 3: the `build_envelope(&MailDispatch)`
// helper retired. Production cap registration closures now take
// `OwnedDispatch` directly and call `Envelope::from(dispatch)` —
// payload + origin move rather than clone. The
// borrowed-dispatch shape is still available through the
// `MailboxEntry::Inline` path elsewhere.

/// Result returned from [`ChassisCtx::claim_mailbox`].
///
/// The capability owns the receiver afterward; the slot is consumed
/// from the registry, so a second claim for the same name fails
/// loud with [`BootError::MailboxAlreadyClaimed`].
///
/// `actor_slots` carries this claim's per-actor [`ActorSlots`] — the
/// chassis [`crate::chassis::builder::Builder::with_actor`] path
/// stamps this into TLS via [`crate::actor::native::local::with_stamped`]
/// around `init` / `wire` / each dispatch so per-actor `Local<T>`
/// lookups (notably the ADR-0081 `ActorLogRing`) resolve to the
/// caller's storage. Driver-as-actor capabilities (issue 603 Phase 3,
/// today only the desktop window driver) that bypass the standard
/// dispatcher slot need to stamp the same slots around their bespoke
/// drain so the framework-built-in `aether.log.tail` /
/// `aether.trace.tail` / `aether.cost.tail` dispatch arms reach the
/// expected ring (iamacoffeepot/aether#1272).
pub struct MailboxClaim {
    pub(crate) id: MailboxId,
    /// ADR-0106: the sealed inbound surface. Replaces the raw
    /// `mpsc::Receiver<Envelope>` the claim used to expose — a capability
    /// reaches inbound envelopes only through the [`SettlingInbox`]'s drain
    /// methods, each of which settles the ADR-0080 §2 bracket on scope
    /// exit. Outside `aether-substrate` it is no longer possible to obtain
    /// an armed [`Envelope`] from a claim.
    pub inbox: SettlingInbox,
    pub actor_slots: SharedActorSlots,
    /// Optional wake hook fired by the registry sink after each
    /// accepted send (iamacoffeepot/aether#1318). Unset when the claim
    /// returns; whoever drains the inbox installs the hook that nudges
    /// its drain. A composed root's boot installs the pool wake that
    /// re-queues the actor's slot, and the desktop window driver, which
    /// drains in `about_to_wait`, installs an `EventLoopProxy` wake so
    /// `aether.window` mail wakes the winit loop even under
    /// `ControlFlow::Wait`.
    pub wake_slot: Arc<MailboxWakeSlot>,
}

impl fmt::Debug for MailboxClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `ActorSlots` doesn't impl `Debug` (interior `RefCell<HashMap>`
        // of type-erased boxes), so hand-roll Debug on `MailboxClaim`
        // and finish non-exhaustively rather than deriving.
        f.debug_struct("MailboxClaim").field("id", &self.id).field("inbox", &self.inbox).finish_non_exhaustive()
    }
}

/// Driver-as-actor's [`ActorSlots`] handle (iamacoffeepot/aether#1272).
///
/// `ActorSlots` uses interior `RefCell` (a single dispatcher thread is
/// the sole owner per ADR-0038), so a bare `Arc<ActorSlots>` is neither
/// `Send` nor `Sync`. Driver-as-actor capabilities access these slots
/// only from their bespoke drain thread (the winit main thread for the
/// desktop window driver), so the interior cell stays effectively
/// single-threaded — mirrors the pooled dispatcher's `PooledSlots`
/// wrapper, but here the access invariant is stricter (one fixed
/// thread, not "at most one pool worker at a time"). The `unsafe impl
/// Sync` / `Send` are the safety story.
#[derive(Clone, Default)]
#[allow(
    clippy::non_send_fields_in_send_ty,
    reason = "driver-as-actor invariant: slots only touched on one fixed thread; see type docs"
)]
pub struct SharedActorSlots(Arc<ActorSlots>);

// SAFETY: see the doc-comment on `SharedActorSlots`. The driver-as-actor
// invariant is that the slots are only ever read inside
// `local::with_stamped` on the driver's bespoke drain thread. No other
// thread holds an `Arc` clone, so the interior `RefCell` accesses are
// single-threaded by construction.
unsafe impl Sync for SharedActorSlots {}
// SAFETY: same justification — single-thread access.
unsafe impl Send for SharedActorSlots {}

impl SharedActorSlots {
    /// Allocate a fresh per-actor slot map.
    #[must_use]
    #[allow(
        clippy::arc_with_non_send_sync,
        reason = "SharedActorSlots's unsafe impl Send/Sync covers this Arc; see type docs"
    )]
    pub fn new() -> Self {
        Self(Arc::new(ActorSlots::new()))
    }

    /// Borrow the inner [`ActorSlots`] for a
    /// [`crate::actor::native::local::with_stamped`] call.
    #[must_use]
    pub fn slots(&self) -> &ActorSlots {
        &self.0
    }
}

/// Cell holding the optional wake hook a `Pooled` mailbox fires after
/// each accepted send. The mailbox's inbox relay holds the
/// `Arc<MailboxWakeSlot>` from the moment its channel is opened; the spawn
/// path populates it once the
/// [`crate::scheduler::Drainable`] slot exists.
#[derive(Default)]
pub struct MailboxWakeSlot {
    inner: OnceLock<MailboxWakeFn>,
}

impl fmt::Debug for MailboxWakeSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxWakeSlot").field("installed", &self.inner.get().is_some()).finish()
    }
}

/// Type-erased wake hook stamped into [`MailboxWakeSlot`].
pub type MailboxWakeFn = Arc<dyn Fn() + Send + Sync + 'static>;

impl MailboxWakeSlot {
    /// Install the wake hook. Idempotent on re-call (silently ignores
    /// the second set), but in production every claim is paired with
    /// a single set.
    pub fn set(&self, fn_: MailboxWakeFn) {
        let _ = self.inner.set(fn_);
    }

    /// Borrow the installed hook. Returns `None` only during the boot
    /// window before the slot is constructed and the hook is set; every
    /// actor is pool-dispatched (issue 1187), so a live actor always has
    /// one installed. Hot path — `OnceLock::get` is a single relaxed
    /// load.
    pub(crate) fn get(&self) -> Option<&MailboxWakeFn> {
        self.inner.get()
    }
}

/// Generic fallback-router handler: a substrate-dispatch hook for a
/// local mailbox-lookup miss. The slot is stored at boot but not yet
/// consulted on the dispatch path, so a locally-unknown mailbox still
/// warn-drops.
///
/// Returning `true` means "I handled this mail" (substrate does nothing
/// further); `false` means "not mine" (substrate falls through to its
/// warn-drop path). The slot is generic and the substrate carries no
/// hub knowledge: a hub reaches an engine by dialing the substrate's
/// `aether_rpc::RpcServerCapability`, and the substrate never dials out,
/// so no hub client claims this slot. Any single capability may (a test
/// router, an alternate fan-out).
pub type FallbackRouter = Arc<dyn Fn(&Envelope) -> bool + Send + Sync + 'static>;

/// Kernel-side handle bundle exposed to a capability during its
/// `boot()` call. Shared (`&mut`) across every `boot()` in the
/// builder — one ctx per build, threaded through the capability list
/// in declaration order (ADR-0070 resolved decision 4).
pub struct ChassisCtx<'a> {
    registry: &'a Arc<Registry>,
    mailer: &'a Arc<Mailer>,
    fallback: &'a mut Option<FallbackRouter>,
    /// Indirection over [`crate::runtime::lifecycle::fatal_abort`]
    /// cloned into every [`crate::NativeBinding`] this ctx builds, so a
    /// wasm-guest trap can fatal-abort the substrate cleanly without
    /// each capability needing to plumb [`crate::HubOutbound`] itself.
    /// Defaults to [`crate::runtime::lifecycle::PanicAborter`] when the
    /// chassis builder doesn't override — production drivers swap in
    /// [`crate::runtime::lifecycle::OutboundFatalAborter`] via
    /// [`crate::chassis::builder::Builder::with_aborter`].
    aborter: &'a Arc<dyn FatalAborter>,
    /// Issue #601: every actor-mailbox claim appends its `MailboxId`
    /// here. The chassis builder reads the list after `boot_passives`.
    ///
    /// Synchronous-handler registrations go through
    /// `Registry::register_inline` directly and do *not* land here —
    /// they're not actors.
    claimed_actor_mailboxes: &'a mut Vec<MailboxId>,
    /// ADR-0155 §4: driver-as-actor mailboxes reserved at the Claim stage
    /// by [`crate::chassis::builder::DriverCapability::claim`] (via
    /// [`Self::claim_driver_mailbox`]), stashed here keyed by namespace so the driver's
    /// Start-stage `boot` can recover the live [`MailboxClaim`] — inbox,
    /// actor slots, wake slot — through
    /// [`crate::chassis::builder::DriverCtx::take_claimed_mailbox`]. The
    /// Claim hook is value-free (it runs at `--describe` time on a headless
    /// host without the driver value), so the reservation it makes cannot
    /// ride the driver struct; it rides this ctx-threaded accumulator
    /// instead, mirroring `claimed_actor_mailboxes`. Empty for every chassis
    /// whose driver claims nothing (the default no-op hook), and drained by
    /// the describe path — which reads only the claimed namespaces off the
    /// registry — when it drops.
    reserved_driver_mailboxes: &'a mut HashMap<String, MailboxClaim>,
    /// Issue 607 Phase 3b (ADR-0079): the chassis's
    /// [`crate::Spawner`], cloned into every booted actor's
    /// [`crate::NativeBinding`] (via [`crate::NativeBinding::from_ctx`])
    /// so per-handler `NativeCtx::spawn_child` can reach the spawn
    /// machinery without separate plumbing. Built once at boot in
    /// `boot_passives`.
    spawner: &'a Arc<crate::Spawner>,
    /// iamacoffeepot/aether#4156: proof that a cap booting through this ctx
    /// may write the registry directly, ahead of the ADR-0165 owner. Minted
    /// per ctx — a ctx exists only inside a boot pass, so the authority
    /// cannot outlive the boot that created it.
    authority: BootAuthority,
    /// ADR-0230: the chassis's record of the root actors it composed. A boot
    /// that publishes a composed actor's `Live` route records its reference
    /// here; the chassis handle's `actor_ref` reads it back by type.
    references: &'a ComposedReferences,
}

/// The borrowed boot accumulators a [`ChassisCtx`] is built over, one per
/// field of the same name.
pub(in crate::chassis) struct ChassisCtxParts<'a> {
    pub(in crate::chassis) registry: &'a Arc<Registry>,
    pub(in crate::chassis) mailer: &'a Arc<Mailer>,
    pub(in crate::chassis) fallback: &'a mut Option<FallbackRouter>,
    pub(in crate::chassis) aborter: &'a Arc<dyn FatalAborter>,
    pub(in crate::chassis) claimed_actor_mailboxes: &'a mut Vec<MailboxId>,
    pub(in crate::chassis) spawner: &'a Arc<crate::Spawner>,
    pub(in crate::chassis) reserved_driver_mailboxes: &'a mut HashMap<String, MailboxClaim>,
    pub(in crate::chassis) references: &'a ComposedReferences,
}

impl<'a> ChassisCtx<'a> {
    /// Internal constructor used by the ADR-0071
    /// [`crate::chassis::builder::Builder`].
    pub(in crate::chassis) fn new(parts: ChassisCtxParts<'a>) -> Self {
        let ChassisCtxParts {
            registry,
            mailer,
            fallback,
            aborter,
            claimed_actor_mailboxes,
            spawner,
            reserved_driver_mailboxes,
            references,
        } = parts;
        Self {
            registry,
            mailer,
            fallback,
            aborter,
            claimed_actor_mailboxes,
            reserved_driver_mailboxes,
            spawner,
            authority: BootAuthority::new(),
            references,
        }
    }

    /// Record the reference a boot minted for the composed root actor `A`
    /// once its `Live` route is published (ADR-0230).
    pub(crate) fn record_reference<A: 'static>(&self, reference: ActorRef<A>) {
        self.references.record(reference);
    }

    /// The reference recorded for the composed root actor `A`, or `None` when
    /// this chassis has composed no `A` so far.
    pub(crate) fn reference<A: 'static>(&self) -> Option<ActorRef<A>> {
        self.references.get::<A>()
    }

    /// Borrow this boot's [`BootAuthority`] — the proof a cap needs to name
    /// the registry's direct mutators (`try_register_inbox_with_id`,
    /// `register_kind_with_descriptor`) while it is still booting. A
    /// handler has no ctx to take this from, which is the point: at steady
    /// state the direct path cannot be named at all
    /// (iamacoffeepot/aether#4156).
    #[must_use]
    pub fn boot_authority(&self) -> &BootAuthority {
        &self.authority
    }

    /// Register a `MailboxEntry::Inbox` under `C::NAMESPACE` and
    /// return both its derived [`MailboxId`] (ADR-0029 hash) and
    /// the receiver. The capability's own type is the single source
    /// of truth for the recipient name (issue 525 Phase 1).
    ///
    /// Tests that need a parameterized name (one fixture, many
    /// claims) reach for [`Self::claim_mailbox_with_override`].
    pub fn claim_mailbox<C: Addressable>(&mut self) -> Result<MailboxClaim, BootError> {
        self.claim_mailbox_with_override(C::NAMESPACE)
    }

    /// Register a `MailboxEntry::Inbox` under `name` and return
    /// both its derived [`MailboxId`] (ADR-0029 hash) and the
    /// inbox. Escape hatch for tests with parameterized names;
    /// production caps go through [`Self::claim_mailbox`] so the
    /// cap's own `NAMESPACE` is authoritative.
    ///
    /// This is the one claim: a composed root actor, a pumped root and a
    /// driver-as-actor mailbox all take it, and differ only in who drains
    /// the inbox afterwards. The relay registered with the registry moves
    /// every delivery onto the claim's inbox. The inbox owns the channel's
    /// only strong sender, so once its owner drops it (a root's shutdown
    /// frees the actor's binding and with it the inbox) whatever was
    /// queued or in the middle of being sent is settled by the inbox's
    /// drop, and a later delivery is settled and warn-logged at the relay.
    pub fn claim_mailbox_with_override(&mut self, name: &str) -> Result<MailboxClaim, BootError> {
        let (receiver, relay) = inbox_channel(self.mailer);
        let wake_slot = Arc::clone(relay.wake_slot());
        let id = self.registry.try_register_inbox(&self.authority, name.to_owned(), relay)?;
        self.claimed_actor_mailboxes.push(id);
        // iamacoffeepot/aether#1272: every claim returns its
        // per-actor [`ActorSlots`] wrapped in [`SharedActorSlots`]. The
        // `with_actor` boot path allocates its own [`ActorSlots`] (via
        // `Box<ActorSlots>` in `ClaimResources`) and ignores this one;
        // driver-as-actor capabilities that own the drain inline (the
        // desktop window driver) wrap their bespoke drain in
        // `local::with_stamped(slots.as_ref(), …)` so framework dispatch
        // arms reach the actor's per-actor `Local<T>` rings.
        Ok(MailboxClaim {
            id,
            inbox: SettlingInbox::new_at(id, receiver, Arc::clone(self.mailer)),
            actor_slots: SharedActorSlots::new(),
            wake_slot,
        })
    }

    /// ADR-0155 §4 Claim-stage reservation for a driver-as-actor mailbox.
    /// The chassis driver's value-free
    /// [`crate::chassis::builder::DriverCapability::claim`] hook calls this
    /// to reserve its inbox during the Claim stage — the same registry
    /// reservation [`Self::claim_mailbox_with_override`] performs — and the
    /// produced [`MailboxClaim`] is stashed on the ctx so the driver's
    /// Start-stage `boot` recovers it via
    /// [`crate::chassis::builder::DriverCtx::take_claimed_mailbox`]. This
    /// splits the registry reservation (Claim, value-free — it runs at
    /// `--describe` time without the driver) from the Start-stage
    /// consumption of the inbox / actor slots / wake slot. The claimed
    /// namespace lands in the registry the same way a passive cap's claim
    /// does, so it appears in the claim-derived describe roster; the
    /// [`MailboxId`] also lands in `claimed_actor_mailboxes`, exactly as it
    /// would have when the driver claimed the inbox inline at Start. A
    /// repeated name fails in the claim with
    /// [`BootError::MailboxAlreadyClaimed`] before it reaches the stash, so a
    /// reservation never overwrites another.
    pub fn claim_driver_mailbox(&mut self, name: &str) -> Result<(), BootError> {
        let claim = self.claim_mailbox_with_override(name)?;
        self.reserved_driver_mailboxes.insert(name.to_owned(), claim);
        Ok(())
    }

    /// Recover a driver-as-actor [`MailboxClaim`] reserved at the Claim
    /// stage by [`Self::claim_driver_mailbox`]. Returns the claim (removing
    /// it from the stash) or `None` when the driver reserved no mailbox
    /// under `name`. Called by
    /// [`crate::chassis::builder::DriverCtx::take_claimed_mailbox`] on the
    /// Start path; a chassis whose driver claims nothing never populates the
    /// stash, so this always returns `None` there.
    pub(crate) fn take_claimed_mailbox(&mut self, name: &str) -> Option<MailboxClaim> {
        self.reserved_driver_mailboxes.remove(name)
    }

    /// Issue 607 Phase 7: undo a previous `claim_*_mailbox` call whose
    /// claim no actor could have observed. Removes the sink's route record
    /// from the chassis registry, so the name can be claimed again
    /// (ADR-0079 §5, ADR-0230 §1: a failed init can be retried under the
    /// same name), and removes the id from `claimed_actor_mailboxes`.
    /// Idempotent: calling on an id that wasn't claimed, or was already
    /// unclaimed, is a no-op.
    ///
    /// Used by a chassis boot that fails before its spawn pass: the claim
    /// or init pass failed, so the whole boot aborts before any dispatcher
    /// runs, and no `wire` mail carries the id. A claim some actor may
    /// have observed is retired instead ([`Self::retire_claim`]).
    pub(crate) fn withdraw_claim(&mut self, id: MailboxId) {
        let _ = self.registry.withdraw_claim(&self.authority, id);
        self.claimed_actor_mailboxes.retain(|i| *i != id);
    }

    /// Undo a previous `claim_*_mailbox` call whose claim some actor may
    /// already have observed: close the id as an actor's close does, less
    /// the actor. The id is tombstoned and its watchers drained, its route
    /// retires to `Dropped`, and each drained watcher is posted its
    /// [`MonitorNotice`](aether_kinds::MonitorNotice), in the close tail's
    /// order. The id leaves `claimed_actor_mailboxes`.
    ///
    /// Used by a pumped boot whose `init` fails while the passives are
    /// dispatching and may hold a `depends` reference to the claim, and may
    /// be watching it. The route keeps its proven name, so such a reference
    /// still names its path (ADR-0230), and the name is never registered
    /// again (ADR-0079 §7). Without the tombstone a later monitor of that
    /// reference would register an entry no close will ever drain. A boot
    /// that fails after an actor's `wire` ran closes the actor first, and
    /// this then finds the id already closed: the tombstone is rewritten,
    /// nothing is drained, and the route is already `Dropped`.
    pub(crate) fn retire_claim(&mut self, id: MailboxId) {
        let watchers = self.registry.actor_registry().close_actor(id);
        let _ = self.registry.drop_mailbox(&self.authority, id);
        post_notices(self.mailer, id, watchers);

        self.claimed_actor_mailboxes.retain(|i| *i != id);
    }

    /// Clone the chassis's `Arc<Mailer>`, the one every actor's binding
    /// routes through. Crate-private: its consumers are the binding
    /// `NativeBinding::from_ctx` builds, `DriverCtx::root_pusher`, and the
    /// passive boot's `init` / `spawn` passes. No capability reaches the
    /// mailer through this ctx.
    #[must_use]
    pub(crate) fn mail_send_handle(&self) -> Arc<Mailer> {
        Arc::clone(self.mailer)
    }

    /// Borrow the chassis's registry. Crate-private: its consumers are the
    /// passive boot's `init` pass (the declared-dependency check) and
    /// `spawn` pass (the seize-handle install).
    #[must_use]
    pub(crate) fn registry(&self) -> &Arc<Registry> {
        self.registry
    }

    /// Clone the chassis's [`FatalAborter`]. Read by the crate-private
    /// `NativeBinding::from_ctx` so the wasm-trap abort
    /// path has somewhere to abort to without each
    /// transport plumbing [`crate::HubOutbound`] itself.
    #[must_use]
    pub fn fatal_aborter(&self) -> Arc<dyn FatalAborter> {
        Arc::clone(self.aborter)
    }

    /// Borrow the chassis's [`crate::Spawner`]. Used by the crate-private
    /// `NativeBinding::from_ctx` to clone an `Arc<Spawner>`
    /// into every booted actor's transport so per-handler
    /// `NativeCtx::spawn_child` can reach the spawn machinery.
    #[must_use]
    pub fn spawner_arc(&self) -> &Arc<crate::Spawner> {
        self.spawner
    }

    /// Issue 635 PR C: borrow the chassis worker pool's wake sink
    /// (ready-queue sender + spin/park coordinator). The `Pooled` branch
    /// of `make_native_actor_boot` clones this into the
    /// [`crate::scheduler::WakeHandle`] that fires when the actor's
    /// mailbox accepts a send.
    pub(crate) fn wake_sink(&self) -> &WakeSink {
        self.spawner.wake_sink()
    }

    /// Install the fallback-router handler. At most one capability
    /// may claim the slot; a second call returns
    /// [`BootError::FallbackRouterAlreadyClaimed`].
    ///
    /// The handler is stored but not yet consulted from substrate
    /// dispatch, so a locally-unknown mailbox still warn-drops. No hub
    /// client claims this slot: a hub reaches an engine by dialing the
    /// substrate's `aether_rpc::RpcServerCapability`, and the substrate
    /// never dials out.
    pub fn claim_fallback_router(&mut self, handler: FallbackRouter) -> Result<(), BootError> {
        if self.fallback.is_some() {
            return Err(BootError::FallbackRouterAlreadyClaimed);
        }
        *self.fallback = Some(handler);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
mod tests {
    use super::*;
    use crate::testing::boot_authority;

    use aether_actor::Local;

    use crate::actor::native::local::with_stamped;
    use aether_kinds::descriptors;

    use aether_data::ErasedActorPath;

    use crate::config::RingCapacities;
    use crate::mail::registry::{DispatchParts, InboxHandler, MailboxEntry, OwnedDispatch};
    use crate::mail::{KindId, MailId, MailRef};
    use crate::runtime::lifecycle::PanicAborter;
    use crate::scheduler::{Pool, PoolConfig, PoolHandle};

    /// Per-actor scratch type for the iamacoffeepot/aether#1272 round-trip
    /// test. Mirrors the `ActorLogRing` shape (`Default + Local`) at the
    /// level the framework dispatch arm reaches it through.
    #[derive(Default)]
    struct Probe(u32);
    impl Local for Probe {}

    /// iamacoffeepot/aether#1272: a `MailboxClaim` returns its per-actor
    /// `ActorSlots`. Driver-as-actor capabilities (today only the desktop
    /// window driver) wrap their bespoke drain in
    /// `local::with_stamped(&claim.actor_slots, …)` so framework dispatch
    /// arms (`aether.log.tail` / `aether.trace.tail` / `aether.cost.tail`)
    /// reach the actor's per-actor `Local<T>` rings.
    #[test]
    fn claim_mailbox_returns_stampable_actor_slots() {
        let (registry, mailer, spawner, aborter, _pool) = test_infra();
        let mut fallback: Option<FallbackRouter> = None;
        let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
        let mut reserved_driver_mailboxes: HashMap<String, MailboxClaim> = HashMap::new();
        let references = ComposedReferences::default();
        let mut ctx = ChassisCtx::new(ChassisCtxParts {
            registry: &registry,
            mailer: &mailer,
            fallback: &mut fallback,
            aborter: &aborter,
            claimed_actor_mailboxes: &mut claimed_actor_mailboxes,
            spawner: &spawner,
            reserved_driver_mailboxes: &mut reserved_driver_mailboxes,
            references: &references,
        });

        let claim = ctx.claim_mailbox_with_override("test.iamacoffeepot.1272.driver").expect("first claim succeeds");

        // Stamp the claim's slots into TLS and write through a `Local<T>`;
        // a second `with_stamped` round-trips reads back through the same
        // slot — the property the framework dispatch arm relies on when
        // it calls `ActorLogRing::try_with(...)` inside the driver's
        // bespoke drain.
        with_stamped(claim.actor_slots.slots(), || {
            Probe::with_mut(|p| p.0 = 0x1272);
        });
        let read_back =
            with_stamped(claim.actor_slots.slots(), || Probe::try_with(|p| p.0).expect("stamped slots carry Probe"));
        assert_eq!(read_back, 0x1272);

        // A fresh `ActorSlots` (not the one carried on the claim) must
        // see the Local at its default — confirms the round-trip above
        // actually went through the claim's slots and not a stale TLS
        // remnant.
        let other = SharedActorSlots::new();
        let other_read =
            with_stamped(other.slots(), || Probe::try_with(|p| p.0).expect("any stamped slots carry Probe"));
        assert_eq!(other_read, 0, "fresh slots see the Local at its default");
    }

    /// A boot that fails before any actor could observe its claim withdraws
    /// it: no record stays behind, and the namespace claims again, so a
    /// corrected retry boots under the same name (ADR-0079 §5). A regression
    /// that retires instead refuses the re-claim.
    #[test]
    fn withdraw_claim_leaves_no_record_and_frees_its_name() {
        let (registry, mailer, spawner, aborter, _pool) = test_infra();
        let name = "test.unclaim.withdraw";

        with_test_ctx(&registry, &mailer, &spawner, &aborter, |ctx| {
            let claim = ctx.claim_mailbox_with_override(name).expect("first claim succeeds");
            ctx.withdraw_claim(claim.id);

            assert!(registry.entry_at(claim.id).is_none(), "the withdrawn claim leaves no record");
            assert!(ctx.claim_mailbox_with_override(name).is_ok(), "the withdrawn name claims again");
        });
    }

    /// A boot unwind some actor may have observed retires the claimed route
    /// instead of deleting it: a reference minted while the claim was `Live`
    /// still answers its actor path afterwards, and the name is spent. A
    /// regression to deletion loses the path (`actor_path` would answer
    /// `None`); a regression to reuse accepts the re-claim.
    #[test]
    fn retire_claim_keeps_its_path_and_spends_its_name() {
        let (registry, mailer, spawner, aborter, _pool) = test_infra();
        let name = "test.unclaim.retire";
        let expected = ErasedActorPath::new(name).expect("the claimed name is a canonical path");

        with_test_ctx(&registry, &mailer, &spawner, &aborter, |ctx| {
            let claim = ctx.claim_mailbox_with_override(name).expect("first claim succeeds");
            let reference = registry.resolve_live(claim.id).expect("the claimed route is live");
            ctx.retire_claim(claim.id);

            assert_eq!(registry.actor_path(reference), Some(expected));
            assert!(!registry.is_live(reference), "the unwound route is no longer live");
            assert!(ctx.claim_mailbox_with_override(name).is_err(), "the retired name never claims again");
        });
    }

    /// A retired claim closes in the lifecycle table as well as in the
    /// routes: its id is tombstoned and its watcher is posted one notice
    /// stamped with it. A route that went `Dropped` with a watcher still
    /// registered would leave that watcher holding state for an actor that
    /// will never close, and with no tombstone a later monitor of the same
    /// reference would register an entry nothing drains.
    #[test]
    fn retire_claim_tombstones_and_notifies_its_watcher() {
        use aether_data::{Kind, Source, SourceAddr};

        let (registry, mailer, spawner, aborter, _pool) = test_infra();

        with_test_ctx(&registry, &mailer, &spawner, &aborter, |ctx| {
            let watcher = ctx.claim_mailbox_with_override("test.unclaim.watcher").expect("the watcher claims");
            let claim = ctx.claim_mailbox_with_override("test.unclaim.watched").expect("the watched claim succeeds");
            assert!(registry.actor_registry().register_monitor(watcher.id, claim.id));
            ctx.retire_claim(claim.id);

            let notice = watcher.inbox.try_next().expect("the watcher is posted the retired claim's notice");
            assert_eq!(notice.kind(), aether_kinds::MonitorNotice::ID);
            assert_eq!(notice.sender(), Source::to(SourceAddr::Component(claim.id)));
            assert!(watcher.inbox.try_next().is_none(), "one notice per registration");
            assert!(registry.actor_registry().is_tombstoned(claim.id), "the retired id is closed");
            assert!(!registry.actor_registry().register_monitor(watcher.id, claim.id), "a later watch is reported");
        });
    }

    /// Run `body` against a fresh `ChassisCtx` over the test infra.
    fn with_test_ctx(
        registry: &Arc<Registry>,
        mailer: &Arc<Mailer>,
        spawner: &Arc<crate::Spawner>,
        aborter: &Arc<dyn FatalAborter>,
        body: impl FnOnce(&mut ChassisCtx<'_>),
    ) {
        let mut fallback: Option<FallbackRouter> = None;
        let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
        let mut reserved_driver_mailboxes: HashMap<String, MailboxClaim> = HashMap::new();
        let references = ComposedReferences::default();

        body(&mut ChassisCtx::new(ChassisCtxParts {
            registry,
            mailer,
            fallback: &mut fallback,
            aborter,
            claimed_actor_mailboxes: &mut claimed_actor_mailboxes,
            spawner,
            reserved_driver_mailboxes: &mut reserved_driver_mailboxes,
            references: &references,
        }));
    }

    /// The long-lived owned infra a `ChassisCtx` borrows from. Held by
    /// the test for the duration of the claim so the registered handler
    /// outlives the `ctx` that registered it.
    type TestInfra = (Arc<Registry>, Arc<Mailer>, Arc<crate::Spawner>, Arc<dyn FatalAborter>, PoolHandle);

    fn test_infra() -> TestInfra {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(&boot_authority(), d);
        }
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
        let pool = Pool::start(PoolConfig::default(), Arc::clone(&aborter));
        let spawner = Arc::new(crate::Spawner::new(
            Arc::clone(&registry),
            Arc::clone(&mailer),
            Arc::clone(&aborter),
            pool.wake_sink(),
            RingCapacities::default(),
        ));
        (registry, mailer, spawner, aborter, pool)
    }

    /// An armed `OwnedDispatch` addressed at `id`, shaped like the
    /// `subscribe_self` mail a loaded component sends from its `wire`
    /// hook. Armed, so dropping it without `discharge`/`mark_transferred`
    /// trips the ADR-0094 guard in a debug build.
    fn armed_subscribe_self(id: MailboxId) -> OwnedDispatch {
        OwnedDispatch::armed(
            DispatchParts {
                mail_id: Some(MailId::new(id, 1)),
                root: Some(MailId::new(id, 1)),
                ..DispatchParts::new(KindId(7), MailRef::from(Vec::new()))
            },
            id,
        )
    }

    /// ADR-0094 / #1564: a claim registers a relay that holds only a weak
    /// sender, so once the root's teardown drops the
    /// claim's inbox the relay refuses. A mail arriving in that window (e.g.
    /// a loaded component's `subscribe_self` racing the lifecycle cap's
    /// teardown) must be settled at the seam, not dropped armed — which
    /// would trip the obligation guard and fatally abort the substrate.
    #[test]
    fn claimed_inbox_settles_the_obligation_when_sender_gone() {
        let (registry, mailer, spawner, aborter, _pool) = test_infra();
        let mut claimed = None;
        with_test_ctx(&registry, &mailer, &spawner, &aborter, |ctx| {
            claimed = Some(ctx.claim_mailbox_with_override("test.1564.sender_gone").expect("claim succeeds"));
        });
        let MailboxClaim { id, inbox, .. } = claimed.expect("the claim was taken");
        // Drop the inbox the way a root's teardown does when it frees the
        // actor's binding: the registry's relay no longer upgrades.
        drop(inbox);

        let Some(MailboxEntry::Inbox { handler, .. }) = registry.entry_at(id) else {
            panic!("claimed mailbox should be an Inbox entry");
        };
        // Pre-fix this dropped the armed dispatch and panicked the guard.
        handler.enqueue(armed_subscribe_self(id));
    }

    /// The happy path: the relay moves the envelope onto the
    /// channel and fires the wake hook. The delivered (armed) dispatch is
    /// settled by draining the inbox — the dispatcher's job in production —
    /// so the obligation guard is satisfied.
    #[test]
    fn the_relay_delivers_and_wakes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let id = MailboxId(0x1565);
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(registry));
        let (receiver, relay) = inbox_channel(&mailer);
        let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));

        let fired = Arc::new(AtomicBool::new(false));
        let fired_for_hook = Arc::clone(&fired);
        relay.wake_slot().set(Arc::new(move || {
            fired_for_hook.store(true, Ordering::SeqCst);
        }));

        relay.enqueue(armed_subscribe_self(id));
        assert!(fired.load(Ordering::SeqCst), "wake hook fired on delivery");
        assert!(inbox.try_next().is_some(), "delivered envelope is on the channel");
    }
}
