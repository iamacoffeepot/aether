use std::any::TypeId;
use std::io;
use std::mem;
use std::sync::{Arc, Weak};

use aether_actor::Root;
use aether_actor::local::ActorSlots;
use aether_actor::log::ActorLogRing;
use aether_actor::trace::ActorTraceRing;

use super::passive_boot::{DynShutdown, PassiveBoot};
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::local;
use crate::actor::native::slot::dispatcher::DispatcherSlot;
use crate::actor::native::{ExportedHandles, NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::ctx::{ChassisCtx, MailboxSender, MailboxWakeSlot};
use crate::chassis::error::BootError;
use crate::config::{ConfigError, ConfigMember, ConfigSources};
use crate::mail::MailboxId;
use crate::mail::cost::CostCells;
use crate::runtime::effect_chain::{EffectChain, Uncaused};
use crate::scheduler::{Drainable, SeizeHandle, WakeHandle};

struct ClaimResources {
    mailbox_id: MailboxId,
    transport: Arc<NativeBinding>,
    mailbox_sender: MailboxSender,
    wake_slot: Arc<MailboxWakeSlot>,
    slots: Box<ActorSlots>,
}

/// Phase state of a [`NativeActorBoot`] — variants carry exactly the
/// resources that phase has acquired. Phase methods transition states
/// via `mem::replace(&mut self.state, Transitioning)` plus a final
/// state assignment, so each transition is atomic w.r.t. partial
/// moves.
///
/// ADR-0156 §5: the cap config + params no longer ride the state — the
/// config is resolved off the builder's source stack in the `resolve`
/// pass (ahead of Claim) and, with the params, held in [`NativeActorBoot`]'s
/// own `Option` fields until `init` consumes them. Decoupling them from
/// the phase state is what lets the claim-only `--describe` path (which
/// never resolves a config value) run the Claim pass on this boot
/// unchanged: `claim` touches neither.
enum BootState<A: Root + NativeActor> {
    /// Pre-claim.
    Pending,
    /// Post-claim, pre-init — mailbox + transport + slots claimed.
    Claimed { resources: ClaimResources },
    /// Post-init, pre-wire — actor instance constructed.
    Initialized { resources: ClaimResources, actor: Box<A::State> },
    /// Post-wire, pre-spawn — wire ran. The dispatcher is next.
    Wired { resources: ClaimResources, actor: Box<A::State> },
    /// Sentinel held only inside a phase method's body between
    /// `mem::replace` and the final state assignment. If the phase
    /// returns Err, it either restores a prior variant (so
    /// [`PassiveBoot::cleanup_after_failure`] sees the right state)
    /// or leaves `Transitioning` when no chassis-side resources are
    /// held (the failed body cleaned up inline).
    Transitioning,
}

/// Issue 552 stage 1 (multi-passed for issue 697): the [`NativeActor`]
/// boot. Claims the cap's mailbox under `A::NAMESPACE`, builds a fresh
/// per-cap [`NativeBinding`], constructs a [`NativeInitCtx`], calls
/// `A::init(config, &mut init_ctx)`, runs `A::wire`, and finally
/// spawns a dispatcher thread that pulls from the transport's inbox
/// and routes through [`Dispatch::dispatch`](crate::actor::native::Dispatch) —
/// the sum dispatch trait the `#[actor] impl NativeActor for A`
/// macro emits.
///
/// ADR-0082 retired the frame-bound claim variant: every cap takes the
/// drop-on-shutdown claim, and settlement gating on the
/// `LifecycleAdvance` chain root (not a per-mailbox pending counter) is
/// the frame-integration gate now.
pub(super) struct NativeActorBoot<A: Root + NativeActor> {
    /// The resolved cap config, filled by the ADR-0156 §5 `resolve` pass off
    /// the builder's source stack and consumed by `init`. `None` until
    /// `resolve` runs — the claim-only `--describe` path never resolves it, so
    /// this stays `None` and the Claim pass runs regardless.
    config: Option<A::Config>,
    /// The ADR-0156 composer-supplied construction input, staged at compose and
    /// consumed by `init`.
    params: Option<A::Params>,
    state: BootState<A>,
}

impl<A: Root + NativeActor> NativeActorBoot<A> {
    pub(super) fn new(params: A::Params) -> Self {
        Self { config: None, params: Some(params), state: BootState::Pending }
    }
}

impl<A: Root + NativeActor> PassiveBoot for NativeActorBoot<A>
where
    A::Config: ConfigMember,
{
    fn resolve(&mut self, sources: &mut ConfigSources) -> Result<(), ConfigError> {
        // ADR-0156 §5: resolve this cap's `Config` off the builder's source
        // stack (programmatic > argv > env > file > default) ahead of Claim.
        // Section identity comes from `A::Config`'s `ConfigMember` declaration,
        // so no chassis-side section string is threaded here.
        self.config = Some(<A::Config as ConfigMember>::resolve(sources)?);
        Ok(())
    }

    fn claim(&mut self, ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        let BootState::Pending = mem::replace(&mut self.state, BootState::Transitioning) else {
            panic!("PassiveBoot::claim called in non-Pending state");
        };

        // Issue 607 Phase 3b (ADR-0079): claim namespace ownership for
        // this singleton's `Addressable::NAMESPACE`. The actor registry
        // tracks one TypeId per namespace across both cardinalities
        // (Singleton/Instanced), so a later `spawn_child::<X>` whose
        // `X::NAMESPACE` collides with this singleton's namespace
        // surfaces as `SpawnError::NamespaceOwnedByOtherType`. Same
        // TypeId re-claiming the same namespace is idempotent.
        if ctx.spawner_arc().actor_registry().try_claim_namespace(A::NAMESPACE, TypeId::of::<A>()).is_err() {
            // The other claim is on the same namespace by a different
            // TypeId — a chassis-build collision. State stays
            // `Transitioning` (no resources held); cleanup_after_failure
            // sees that and does nothing.
            return Err(BootError::Other(Box::new(io::Error::other(format!(
                "namespace {:?} already owned by a different TypeId — fix the conflicting actor's NAMESPACE const",
                A::NAMESPACE
            )))));
        }

        // ADR-0082: every cap takes the drop-on-shutdown claim. The
        // FRAME_BARRIER frame-bound claim variant retired with the
        // per-frame drain barrier — settlement gating on the
        // LifecycleAdvance chain root is the frame-integration gate
        // now.
        let claim_result = ctx
            .claim_mailbox_drop_on_shutdown::<A>()
            .map(|claim| (claim.id, claim.receiver, claim.mailbox_sender, claim.wake_slot));
        let (mailbox_id, receiver, mailbox_sender, wake_slot) = match claim_result {
            Ok(c) => c,
            Err(e) => {
                // Release the namespace claim we just made — otherwise
                // a later cap with a different TypeId legitimately
                // claiming the same namespace can't (issue 607 Phase 7).
                ctx.spawner_arc().actor_registry().release_namespace(A::NAMESPACE, TypeId::of::<A>());
                // State stays `Transitioning` — no further cleanup
                // for the rollback loop to do.
                return Err(e);
            }
        };

        // Per-cap transport. `NativeBinding::from_ctx` pulls the
        // chassis's aborter + spawner.
        let transport = Arc::new(NativeBinding::from_ctx::<A>(ctx, mailbox_id));
        transport.install_inbox(receiver);

        // Per-actor scratch storage (issue 582 / ADR-0074). Stamped
        // into TLS via `local::with_stamped` for the duration of
        // `init`, `wire`, and each handler dispatch so library code
        // inside the actor (e.g., the issue-581 log buffer) can reach
        // `Local::with_mut` without threading a ctx through.
        let slots = Box::new(ActorSlots::new());
        // Issue 1990: seed the per-actor rings at the chassis-wide
        // configured capacities, read off the shared `Spawner` (the
        // single source). Mirrors the instanced spawn funnel in
        // `Spawner::spawn_actor`.
        let ring_capacities = ctx.spawner_arc().ring_capacities();
        slots.seed(ActorLogRing::with_capacity(ring_capacities.log));
        slots.seed(ActorTraceRing::with_growth(ring_capacities.trace, ring_capacities.trace_max));

        self.state = BootState::Claimed {
            resources: ClaimResources { mailbox_id, transport, mailbox_sender, wake_slot, slots },
        };
        Ok(())
    }

    fn init(&mut self, ctx: &mut ChassisCtx<'_>, handles: &mut ExportedHandles) -> Result<(), BootError> {
        let BootState::Claimed { resources } = mem::replace(&mut self.state, BootState::Transitioning) else {
            panic!("PassiveBoot::init called in non-Claimed state");
        };
        // ADR-0156 §5: the config was resolved in the `resolve` pass; params
        // were staged at compose. Both are consumed here.
        let config = self.config.take().expect("PassiveBoot::init requires the resolve pass to have run first");
        let params = self.params.take().expect("PassiveBoot::init called twice — params already consumed");

        // ADR-0081: wrap `init` in `local::with_stamped` so any
        // `tracing::*` event the cap fires lands in its per-actor
        // `ActorLogRing`. The pre-ADR `with_actor_dispatch` +
        // `drain_buffer` flush hop retired alongside `LogBatch`.
        let init_result = {
            let mailer_clone = ctx.mail_send_handle();
            let mut init_ctx = NativeInitCtx::new(&resources.transport, handles, mailer_clone);
            local::with_stamped(&resources.slots, || A::init(config, params, &mut init_ctx))
        };
        let actor = match init_result {
            Ok(a) => a,
            Err(e) => {
                // A::init consumed `config` + `params`, so we can't restore
                // the Claimed variant. Inline the same cleanup
                // `cleanup_after_failure` would do for Claimed: release
                // the mailbox + namespace claim, then let `resources`
                // drop at end of scope (closing transport + sender).
                ctx.unclaim_mailbox(resources.mailbox_id);
                ctx.spawner_arc().actor_registry().release_namespace(A::NAMESPACE, TypeId::of::<A>());
                drop(resources);
                // State stays `Transitioning` — no further work for
                // the rollback loop to do.
                return Err(e);
            }
        };

        // iamacoffeepot/aether#1037: register this native cap's ADR-0033
        // receive-side capabilities (handler kinds + `#[fallback]`
        // presence) into the queryable `CapabilityRegistry`, the same
        // population path a wasm component's load takes. `A` is a
        // `Dispatch`, whose `capabilities` the `#[actor]`
        // macro overrides to enumerate the cap's handlers; the default
        // (empty) covers any cap the macro didn't touch.
        let capabilities = A::capabilities();
        ctx.mail_send_handle().capability_registry().register(resources.mailbox_id, &capabilities);

        // iamacoffeepot/aether#1128: seed this native cap's per-handler
        // cost cells into the global `CostTable` (same hook as the
        // cap-registry accept-set above), then stamp the same `Arc`s
        // into the actor's per-actor `CostCells` cache. Unlike the wasm
        // load path (cap-thread, can't reach the trampoline's slots), a
        // native cap's `slots` are right here — wrap the cache seed in
        // `with_stamped(&resources.slots, ...)` exactly like the `init`
        // wrap above so both indexes share the same neutral cells.
        // iamacoffeepot/aether#4266: seed from the dispatched set, not the
        // advertised one — `capabilities` above still feeds the registry.
        let handler_kinds: Vec<aether_data::KindId> = A::measured_kinds();
        let seeded = ctx.mail_send_handle().cost_table().seed(resources.mailbox_id, &handler_kinds);
        local::with_stamped(&resources.slots, || {
            use aether_actor::Local as _;
            CostCells::try_with_mut(|cells| cells.seed(seeded));
        });

        // Issue 629 / Phase A: dispatcher takes Box<A> ownership.
        self.state = BootState::Initialized { resources, actor: Box::new(actor) };
        Ok(())
    }

    fn wire(&mut self) -> Result<(), BootError> {
        let BootState::Initialized { resources, mut actor } = mem::replace(&mut self.state, BootState::Transitioning)
        else {
            panic!("PassiveBoot::wire called in non-Initialized state");
        };

        // Issue 584 Phase 2a (ADR-0079 amended): post-init mail-allowed
        // hook. The wire pass runs after the chassis's claim + init
        // passes, so every peer mailbox is published and addressable;
        // wire-emitted mail queues in recipient inboxes (no dispatcher
        // is running yet — spawn pass is next). Wrapped in the same
        // `with_stamped` envelope as `init` and per-envelope dispatch
        // so `Local<T>` and `tracing::*` route into this actor's
        // `ActorLogRing` identically.
        local::with_stamped(&resources.slots, || {
            let mut wire_ctx = NativeCtx::for_wire(&resources.transport, EffectChain::Uncaused(Uncaused::ChassisBoot));
            A::wire(actor.as_mut(), &mut wire_ctx);
        });

        self.state = BootState::Wired { resources, actor };
        Ok(())
    }

    fn spawn(self: Box<Self>, ctx: &mut ChassisCtx<'_>) -> Result<Box<dyn DynShutdown>, BootError> {
        let BootState::Wired { resources, actor } = self.state else {
            panic!("PassiveBoot::spawn called in non-Wired state");
        };
        let ClaimResources { mailbox_id, transport, mailbox_sender, wake_slot, slots } = resources;

        // Register a `DispatcherSlot` with the chassis worker pool. No
        // per-actor thread (issue 635 Phase 3 made `Pooled` the only
        // path; issue 1187 removed the `Dedicated` opt-out). The
        // `wake_slot` in the mailbox closure fires the pool wake hook on
        // every accepted send.
        let actor_registry = Arc::clone(ctx.spawner_arc().actor_registry());
        let mailer_clone = ctx.mail_send_handle();
        let slot =
            DispatcherSlot::<A>::new(actor, Arc::clone(&transport), slots, actor_registry, mailer_clone, mailbox_id);
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let weak: Weak<dyn Drainable> = Arc::downgrade(&slot_dyn);
        // iamacoffeepot/aether#1135: surface the seize handle on this
        // actor's `Inbox` entry so the blob demuxer can dispatch its
        // fan-out in place rather than depositing + repop'ing through the
        // inbox. Same `(state, weak)` pair the wake handle carries; the
        // registry owns the strong slot ref, so the demuxer's `Weak`
        // upgrade fails cleanly after teardown.
        ctx.registry().install_seize_handle(
            ctx.boot_authority(),
            mailbox_id,
            SeizeHandle::new(Arc::clone(slot.state()), Arc::downgrade(&slot_dyn)),
        );
        drop(slot_dyn);
        let wake = WakeHandle::new(Arc::clone(slot.state()), weak, ctx.wake_sink().clone());
        // Issue 697 multi-pass: mail addressed at this actor during the
        // wire pass landed in its inbox before the wake hook was
        // installed, so the closure-side wake fired against an empty
        // `wake_slot`. Fire one wake here so a populated inbox enters the
        // ready queue. Mirrors the same fix `Spawner::spawn_actor`'s
        // Pooled branch carries (issue 635 Phase 3).
        let manual_wake = wake.clone();
        wake_slot.set(Arc::new(move || {
            // Inbox-sender hook — same fire-and-forget shape as the
            // spawn.rs analogue: scheduler deduplicates the CAS, so the
            // bool is irrelevant here.
            let _ = wake.wake();
        }));
        let _ = manual_wake.wake();
        Ok(Box::new(PooledActorShutdown::<A> { slot: Some(slot), mailbox_sender: Some(mailbox_sender) })
            as Box<dyn DynShutdown>)
    }

    fn cleanup_after_failure(self: Box<Self>, ctx: &mut ChassisCtx<'_>) {
        match self.state {
            // Pre-claim or mid-method failure that already cleaned up
            // inline — no chassis-side state to release.
            BootState::Pending | BootState::Transitioning => {}
            // Any past-claim variant: release the mailbox + namespace
            // claims. `resources` (and any held actor) drop at the end
            // of this match arm — dropping `transport` closes the
            // installed receiver, dropping `mailbox_sender` closes the
            // channel.
            BootState::Claimed { resources, .. }
            | BootState::Initialized { resources, .. }
            | BootState::Wired { resources, .. } => {
                ctx.unclaim_mailbox(resources.mailbox_id);
                ctx.spawner_arc().actor_registry().release_namespace(A::NAMESPACE, TypeId::of::<A>());
            }
        }
    }
}

/// Shutdown adapter for a `Pooled` [`NativeActor`] (issue 635 PR C).
/// On chassis shutdown:
/// 1. Sets the binding's `should_shutdown` flag so the next
///    [`crate::scheduler::DispatcherSlot::run_cycle`] observes the
///    signal and runs `unwire` + registry finalize.
/// 2. Drops the [`MailboxSender`] so subsequent
///    sends warn-and-discard.
/// 3. Drops the slot Arc — the chassis-held strong ref. The pool
///    worker's strong ref (via the ready queue) drops at end of the
///    final cycle. The pool's `Drop` joins workers, so any in-flight
///    cycle finishes before chassis shutdown returns.
///
/// Every actor drains on the pool (issue 635 Phase 3 made `Pooled` the
/// default; issue 1187 removed the `Dedicated` opt-out), so this is the
/// runtime shutdown path for every chassis cap.
struct PooledActorShutdown<A>
where
    A: NativeActor,
{
    slot: Option<Arc<DispatcherSlot<A>>>,
    mailbox_sender: Option<MailboxSender>,
}

impl<A> DynShutdown for PooledActorShutdown<A>
where
    A: NativeActor,
{
    fn shutdown_dyn(mut self: Box<Self>) {
        if let Some(slot) = &self.slot {
            slot.binding().signal_shutdown();
        }
        // Drop sender first so the inbox closes; subsequent wakes
        // silently no-op via WakeHandle's Weak failing to upgrade.
        self.mailbox_sender.take();
        drop(self.slot.take());
    }
}
