//! Committing a prepared birth, by whichever route the ADR-0165 seal leaves
//! open.
//!
//! Before the seal the boot path still holds the `Spawner`'s
//! [`BootAuthority`], so `commit_directly` performs every shared write and
//! lifecycle action in the established order on the calling thread. After
//! it, no token exists and `commit_through_owner` submits the birth to the
//! registry owner and blocks on its decision — safe precisely because the
//! caller there is an external embedder thread, never a pool worker.

use std::any::TypeId;
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::Duration;

use aether_actor::Instanced;

use crate::actor::native::envelope::Envelope;
use crate::actor::native::local;
use crate::actor::native::slot::dispatcher::DispatcherSlot;
use crate::actor::native::spawn::activation::NativeSpawnFinalizer;
use crate::actor::native::{NativeActor, NativeCtx};
use crate::chassis::ctx::{MailboxWakeSlot, RelayOutcome, relay_or_transfer};
use crate::mail::cost::CostCells;
use crate::mail::registry::effect::{EffectBatch, RegistryEffect};
use crate::mail::registry::{BootAuthority, NameConflict, OwnedDispatch};
use crate::mail::{KindId, MailboxId};
use crate::runtime::effect_chain::{EffectChain, Uncaused};
use crate::scheduler::{Drainable, SeizeHandle, WakeHandle};

use super::super::{SpawnError, SpawnOutcome};
use super::prepare::{SpawnIdentity, StagedActor};
use super::{InstancedSlotEntry, Spawner};

/// How long a post-seal external spawn waits for the birth it submitted to be
/// decided. It covers both legs — the owner accepting the batch and the
/// activation reaching `Live` at its scheduler home — which take two pool
/// turns on a healthy substrate, so anything approaching this budget is a
/// wedged pool rather than a slow one.
const BIRTH_PATIENCE: Duration = Duration::from_secs(30);
pub(in crate::actor::native::spawn) struct SpawnCommit {
    pub(in crate::actor::native::spawn) mailbox_id: MailboxId,
    pub(in crate::actor::native::spawn) canonical_name: String,
}

impl Spawner {
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
    pub(in crate::actor::native::spawn) fn commit<A>(
        self: Arc<Self>,
        staged: StagedActor<A>,
    ) -> Result<SpawnCommit, SpawnError>
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
        let finalizer = NativeSpawnFinalizer::external(
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
                    RelayOutcome::SenderGone { kind } => {
                        tracing::warn!(
                            target: "aether_substrate::spawn",
                            kind = %kind,
                            "instanced actor sender dropped — mail discarded"
                        );
                    }
                    RelayOutcome::ReceiverGone { kind } => {
                        tracing::warn!(
                            target: "aether_substrate::spawn",
                            kind = %kind,
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
