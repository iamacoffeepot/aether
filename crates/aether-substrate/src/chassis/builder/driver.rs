use std::any::{Any, TypeId};
use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::sync::Arc;

use aether_actor::Root;
use aether_actor::local::ActorSlots;
use aether_actor::log::ActorLogRing;
use aether_actor::trace::ActorTraceRing;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::local;
use crate::actor::native::pumped_slot::PumpedSlot;
use crate::actor::native::{ExportedHandles, NativeActor, NativeCtx, NativeInitCtx};
use crate::actor::registry::ActorRegistry;
use crate::chassis::ctx::{ChassisCtx, FallbackRouter, MailboxClaim, MailboxWakeSlot};
use crate::chassis::error::BootError;
use crate::chassis::inbox::SettlingInbox;
use crate::config::ConfigMemberRecord;
use crate::mail::cost::CostCells;
use crate::mail::mailer::Mailer;
use crate::mail::{MailId, MailboxId, Source};

#[derive(Debug)]
pub enum RunError {
    Other(Box<dyn StdError + Send + Sync + 'static>),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(e) => write!(f, "driver run failed: {e}"),
        }
    }
}

impl StdError for RunError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Other(e) => Some(&**e),
        }
    }
}

/// A driver capability owns the chassis main thread. Each chassis
/// composes exactly one driver alongside its passive capabilities.
/// The driver's [`DriverRunning::run`] body holds whatever loop the
/// chassis needs — winit on desktop, std-timer on headless, TCP
/// accept on hub.
///
/// Not `Send`: the desktop driver's `winit::EventLoop` is `!Send` on
/// macOS, so the driver and its running stay on the chassis main
/// thread end-to-end. The `Builder` holds the driver capability and
/// the resulting `Running` on a single-threaded code path between
/// [`Builder::driver`](crate::chassis::builder::Builder::driver) and
/// [`BuiltChassis::run`](crate::chassis::builder::BuiltChassis::run), so
/// neither needs to cross threads.
pub trait DriverCapability: 'static {
    type Running: DriverRunning;

    /// ADR-0155 Claim-stage hook: reserve every mailbox/namespace this
    /// driver owns as a driver-as-actor (ADR-0071 phase 3), using only
    /// the driver *type* — no driver value, no runtime handles. This is
    /// an associated function rather than a method precisely because the
    /// desktop driver's winit `EventLoop` does not (and cannot) exist at
    /// claim time: `--describe` captures the capability roster on a
    /// headless host, so the hook must run without constructing the
    /// driver. Claims land on the passed [`ChassisCtx`] the same way a
    /// passive cap's claim does, so the driver's namespaces appear in the
    /// claim-derived roster alongside the `with_actor` chain and inline
    /// sinks. Called by
    /// [`Builder::claim_namespaces`](crate::chassis::builder::Builder::claim_namespaces).
    ///
    /// Default: claims nothing. Headless, hub, and the passive
    /// [`NeverDriver`] own no driver-as-actor mailbox; only the desktop
    /// driver (which serves `aether.window`) overrides this. It reserves the
    /// inbox here with [`ChassisCtx::claim_driver_mailbox`], splitting the
    /// registry reservation (Claim) from the Start-stage consumption of the
    /// inbox / actor slots / `EventLoop`-proxy wake — which [`Self::boot`]
    /// recovers via [`DriverCtx::take_claimed_mailbox`] (ADR-0155 §4 / issue
    /// #3834). Both the fused build path and the value-free `--describe`
    /// path run this hook, so `aether.window` appears in the claim-derived
    /// roster while the driver value is never constructed.
    fn claim(ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        let _ = ctx;
        Ok(())
    }

    /// ADR-0156 §4: the driver's operator-resolvable config members — the
    /// tick knob for the headless timer driver, the window knobs for the
    /// desktop driver. Type-level like [`Self::claim`] (no driver value, no
    /// runtime handles), so
    /// [`Builder::config_manifest`](crate::chassis::builder::Builder::config_manifest)
    /// folds them into the aggregate without constructing the driver — the
    /// same reason the tick knob belongs to the headless driver and the
    /// window knobs to the desktop driver rather than a shared chassis list.
    /// Default: no members (hub / `NeverDriver` own no operator config).
    #[must_use]
    fn config_members() -> Vec<ConfigMemberRecord> {
        Vec::new()
    }

    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError>;
}

/// Post-boot driver handle. Built once at chassis boot, then handed
/// to [`BuiltChassis::run`](crate::chassis::builder::BuiltChassis::run),
/// which calls [`DriverRunning::run`] on the calling thread. Returns
/// when the underlying loop drains
/// cleanly (window closed, accept loop done, shutdown signal).
pub trait DriverRunning: 'static {
    fn run(self: Box<Self>) -> Result<(), RunError>;
}

/// Phantom [`DriverCapability`] for passive chassis (substrate-harness, future
/// embedder-driven chassis kinds). The [`Chassis`](crate::chassis::Chassis)
/// trait requires `type Driver: DriverCapability`; passive chassis
/// declare this as their driver to satisfy the bound, but the value is
/// never instantiated (the `Builder<C, NoDriver>` path produces a
/// [`PassiveChassis<C>`](crate::chassis::builder::PassiveChassis) without
/// ever resolving `C::Driver`). Its `boot` is `unreachable!()` — reaching
/// it implies someone tried to drive a
/// chassis that has no driver, which is a programmer error rather than
/// a runtime condition.
pub struct NeverDriver;

impl DriverCapability for NeverDriver {
    type Running = NeverDriverRunning;
    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        unreachable!(
            "NeverDriver is a phantom for passive chassis; it should never be booted. \
             Build the chassis via its inherent `build_passive(env)` instead."
        );
    }
}

/// Running-side of [`NeverDriver`]; same unreachability contract.
pub struct NeverDriverRunning;

impl DriverRunning for NeverDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        unreachable!("NeverDriverRunning::run is never called by design");
    }
}

/// Boot-time context handed to a [`DriverCapability`]. Forwards the
/// passive [`ChassisCtx`] surface; pre-PR-E3 it also exposed typed
/// access to passive runnings via `expect` / `try_get`, but the
/// typed-runnings map retired alongside `Capability` so drivers
/// wanting cap state get it through pre-build accessors (a cap-published
/// handle bundle like `HttpServerHandle`).
///
/// Issue 629 / Phase A: borrows the chassis's [`ExportedHandles`]
/// map. Drivers retrieve cap-published handle bundles via
/// [`Self::handle`]. The pre-629 `actor::<A>() -> Arc<A>` accessor
/// retired — the actor itself never escapes its dispatcher thread, so
/// drivers consume cap-exported handle clones instead.
pub struct DriverCtx<'a> {
    inner: ChassisCtx<'a>,
    handles: &'a ExportedHandles,
}

impl<'a> DriverCtx<'a> {
    pub(super) fn new(inner: ChassisCtx<'a>, handles: &'a ExportedHandles) -> Self {
        Self { inner, handles }
    }

    /// Drivers have no `NAMESPACE` const to delegate against — claim
    /// by explicit name.
    pub fn claim_mailbox(&mut self, name: &str) -> Result<MailboxClaim, BootError> {
        self.inner.claim_mailbox_with_override(name)
    }

    /// ADR-0155 §4: recover a driver-as-actor [`MailboxClaim`] the driver's
    /// [`DriverCapability::claim`] hook reserved at the Claim stage under
    /// `name`. Returns the live claim — inbox, actor slots, wake slot, id —
    /// so the Start-stage [`DriverCapability::boot`] can take ownership of
    /// the inbox and install its runtime wiring (e.g. the desktop driver's
    /// `EventLoopProxy` wake on `aether.window`), rather than re-claiming
    /// the mailbox (which would collide with the Claim-stage reservation).
    /// `None` when the driver's claim hook reserved no mailbox under `name`.
    pub fn take_claimed_mailbox(&mut self, name: &str) -> Option<MailboxClaim> {
        self.inner.take_claimed_mailbox(name)
    }

    #[must_use]
    pub fn mail_send_handle(&self) -> Arc<Mailer> {
        self.inner.mail_send_handle()
    }

    pub fn claim_fallback_router(&mut self, handler: FallbackRouter) -> Result<(), BootError> {
        self.inner.claim_fallback_router(handler)
    }

    /// Issue 629 / Phase A: retrieve a clone of a cap-published handle
    /// bundle of type `H`. `None` if no cap published one (typically
    /// because the cap that owns the handle wasn't booted on this
    /// chassis). Drivers use this to pull `HttpServerHandle` and similar
    /// driver-facing sub-handle bundles without reaching for the cap
    /// itself.
    #[must_use]
    pub fn handle<H: Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
        self.handles.get::<H>()
    }

    /// ADR-0160 §1: boot a [`PumpedSlot`] for driver-as-actor `A` from the
    /// Claim-stage reservation the driver's
    /// [`DriverCapability::claim`] hook made under `A::NAMESPACE`. Recovers
    /// that [`MailboxClaim`], claims the namespace `TypeId`, builds a
    /// [`NativeBinding`], installs the claim's inbox re-lineaged onto the
    /// binding's disjoint reply-id space, seeds the per-actor rings + cost
    /// cache, registers `A::capabilities()`, and runs `init` / `wire` under
    /// `with_stamped` — the exact sequence `NativeActorBoot` runs for a
    /// pooled actor, so `describe` and `actor_cost` behave identically.
    /// Returns the slot
    /// plus the claim's [`MailboxWakeSlot`], so the driver installs its own
    /// wake (desktop: an `EventLoopProxy` poke) that nudges its pump cadence
    /// when mail arrives while the loop is parked.
    ///
    /// The dispatch drain itself is chassis-owned: the driver calls
    /// [`PumpedSlot::drain_available`] at its pump point and
    /// [`PumpedSlot::shutdown`] on exit. Only the dispatch *semantics*
    /// (the shared `dispatch_envelope` body) is framework-owned.
    ///
    /// Errors if `A::NAMESPACE` is already owned by a different actor type,
    /// or if the driver reserved no Claim-stage mailbox under it (its
    /// `claim` hook must call
    /// [`ChassisCtx::claim_driver_mailbox`](crate::chassis::ctx::ChassisCtx::claim_driver_mailbox)),
    /// or if `A::init` returns `Err` — in every failure the namespace + any
    /// mailbox claim are released before returning.
    pub fn boot_pumped_actor<A>(
        &mut self,
        config: A::Config,
        params: A::Params,
    ) -> Result<(PumpedSlot<A>, Arc<MailboxWakeSlot>), BootError>
    where
        A: Root + NativeActor,
    {
        // Claim namespace ownership for this actor's `NAMESPACE` (mirrors
        // `NativeActorBoot::claim`), so a later collision surfaces loud.
        if self.inner.spawner_arc().actor_registry().try_claim_namespace(A::NAMESPACE, TypeId::of::<A>()).is_err() {
            return Err(BootError::Other(Box::new(io::Error::other(format!(
                "namespace {:?} already owned by a different TypeId — fix the conflicting actor's NAMESPACE const",
                A::NAMESPACE
            )))));
        }

        // Recover the ADR-0155 §4 Claim-stage reservation the driver's
        // `claim` hook made under `A::NAMESPACE`.
        let Some(claim) = self.take_claimed_mailbox(A::NAMESPACE) else {
            self.inner.spawner_arc().actor_registry().release_namespace(A::NAMESPACE, TypeId::of::<A>());
            return Err(BootError::Other(Box::new(io::Error::other(format!(
                "no Claim-stage driver mailbox reserved under {:?} — the driver's `claim` hook must call \
                 `claim_driver_mailbox`",
                A::NAMESPACE
            )))));
        };
        let MailboxClaim { id: mailbox_id, inbox, wake_slot, .. } = claim;

        // ADR-0160 §1 / ADR-0161 R4: the binding + inbox install + seed +
        // init/wire + slot assembly is shared with the passive pumped boot
        // ([`crate::chassis::builder::PassiveChassis::boot_pumped_actor`]).
        // The `Spawner` carries the chassis mailer / aborter / actor-registry
        // / ring capacities the assembly needs — the same handles
        // `mail_send_handle` / `fatal_aborter` would source.
        let slot = match assemble_pumped_slot::<A>(mailbox_id, inbox, self.inner.spawner_arc(), config, params) {
            Ok(slot) => slot,
            Err(e) => {
                self.inner.unclaim_mailbox(mailbox_id);
                self.inner.spawner_arc().actor_registry().release_namespace(A::NAMESPACE, TypeId::of::<A>());
                return Err(e);
            }
        };
        Ok((slot, wake_slot))
    }
}

/// Assemble a [`PumpedSlot<A>`] from an already-claimed mailbox + inbox
/// (ADR-0160 §1 / ADR-0161 R4). Builds the per-cap [`NativeBinding`],
/// installs the claim's inbox re-lineaged onto the binding's disjoint
/// reply-id space (issue 1695), seeds the per-actor rings + cost cache at
/// the chassis-wide capacities, and runs `init` / `wire` under
/// `with_stamped` — the exact sequence `NativeActorBoot` runs for a pooled
/// actor, so `describe` and `actor_cost` behave identically. Shared by
/// [`DriverCtx::boot_pumped_actor`] (which recovers a driver's Claim-stage
/// reservation) and [`crate::chassis::builder::PassiveChassis::boot_pumped_actor`]
/// (which claims a fresh mailbox post-boot on a no-driver chassis).
///
/// The namespace claim + its release-on-error are the caller's: this
/// function only reports `init` failure back through its `Err` so the caller
/// can unwind whatever registry / namespace state it reserved. Every other
/// chassis handle (mailer, aborter, actor registry, ring capacities) is
/// sourced from the `spawner`, keeping the arg list to the per-actor inputs.
#[allow(
    clippy::redundant_pub_crate,
    reason = "crate-internal boot helper shared across the private driver / built modules"
)]
pub(crate) fn assemble_pumped_slot<A>(
    mailbox_id: MailboxId,
    inbox: SettlingInbox,
    spawner: &Arc<crate::Spawner>,
    config: A::Config,
    params: A::Params,
) -> Result<PumpedSlot<A>, BootError>
where
    A: Root + NativeActor,
{
    let mailer = spawner.mailer();
    let ring_capacities = spawner.ring_capacities();
    // Per-cap transport; install the claim's inbox re-lineaged onto the
    // binding's disjoint reply-id space (ADR-0160 §1 / issue 1695). A
    // root-pinned chassis capability (depth-1), so its lineage carry is its
    // own `ActorId.0` == `mailbox_id.0`.
    let transport = Arc::new(NativeBinding::new::<A>(
        Arc::clone(mailer),
        mailbox_id,
        mailbox_id.0,
        Arc::from(A::NAMESPACE),
        Arc::clone(spawner.aborter()),
        Some(Arc::clone(spawner)),
    ));
    transport.install_settling_inbox(inbox.relineage(transport.reply_lineage()));

    // Fresh per-actor slots, rings seeded at the chassis-wide capacities —
    // the exact `NativeActorBoot::claim` block.
    let slots = Box::new(ActorSlots::new());
    slots.seed(ActorLogRing::with_capacity(ring_capacities.log));
    slots.seed(ActorTraceRing::with_growth(ring_capacities.trace, ring_capacities.trace_max));

    // `init` under `with_stamped`. A driver-as-actor does not publish a
    // cross-thread handle bundle (the window actor's cell rides its
    // `Params`), so a local `ExportedHandles` suffices here.
    let mut handles = ExportedHandles::new();
    let init_result = {
        let mut init_ctx = NativeInitCtx::new(&transport, &mut handles, Arc::clone(mailer));
        local::with_stamped(&slots, || A::init(config, params, &mut init_ctx))
    };
    let mut actor = Box::new(init_result?);

    // Register capabilities + seed the cost table + stamp the per-actor
    // `CostCells` — the exact `NativeActorBoot::init` block, so `describe` /
    // `actor_cost` behave identically.
    let capabilities = A::capabilities();
    mailer.capability_registry().register(mailbox_id, &capabilities);
    let handler_kinds: Vec<aether_data::KindId> = capabilities.handlers.iter().map(|h| h.id).collect();
    let seeded = mailer.cost_table().seed(mailbox_id, &handler_kinds);
    local::with_stamped(&slots, || {
        use aether_actor::Local as _;
        CostCells::try_with_mut(|cells| cells.seed(seeded));
    });

    // `wire` under `with_stamped` — mail-allowed, so subscriptions land.
    local::with_stamped(&slots, || {
        let mut wire_ctx = NativeCtx::new(&transport, Source::NONE, MailId::NONE, MailId::NONE);
        A::wire(actor.as_mut(), &mut wire_ctx);
    });

    let actor_registry: Arc<ActorRegistry> = Arc::clone(spawner.actor_registry());
    Ok(PumpedSlot::new(actor, transport, slots, actor_registry, mailbox_id))
}
