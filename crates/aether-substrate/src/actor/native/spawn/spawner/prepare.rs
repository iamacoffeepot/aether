//! Preparing a birth: resolve its identity, construct and initialize the
//! actor, and erase it into an owner commit.
//!
//! Nothing here writes shared state. `prepare_identity` folds the lineage
//! (ADR-0099 §3) and `build` runs `A::init` on the calling thread, so a
//! failure drops the partial state before anything is staged; `preflight`
//! adds the legacy eager namespace/tombstone checks that handler staging
//! deliberately leaves to owner-time reservation. `prepare_commit` is the
//! seam to the registry owner — everything past it is [`super::commit`]'s.

use std::any::TypeId;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use aether_actor::local::ActorSlots;
use aether_actor::log::ActorLogRing;
use aether_actor::trace::ActorTraceRing;
use aether_actor::{Instanced, validate_namespace_segment};
use aether_data::{ActorId, Tag, fold_lineage, with_tag};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::actor::native::local;
use crate::actor::native::spawn::activation::{LegacyPreparedActivation, NativeSpawnFinalizer};
use crate::actor::native::{ExportedHandles, NativeActor, NativeInitCtx};
use crate::mail::cost::{CostCell, CostCells};
use crate::mail::registry::effect::{PreparedCostCells, PreparedMail, PreparedRoute, PreparedSpawnCommit};
use crate::mail::{KindId, Mail, MailboxId};
use crate::runtime::effect_chain::EffectChain;

use super::super::{SpawnError, Subname};
use super::Spawner;

/// Identity resolved before construction starts. The canonical name is a
/// display/reverse-map value; `id` remains the lineage-folded route key.
pub(in crate::actor::native::spawn) struct SpawnIdentity {
    pub(in crate::actor::native::spawn) id: MailboxId,
    pub(in crate::actor::native::spawn) parent: MailboxId,
    pub(in crate::actor::native::spawn) carry: u64,
    pub(in crate::actor::native::spawn) canonical_name: Arc<str>,
    pub(in crate::actor::native::spawn) subname: String,
}

/// Private prepared birth. It deliberately owns the initialized state rather
/// than committing a storage representation to the builder API.
pub(in crate::actor::native::spawn) struct StagedActor<A: NativeActor> {
    pub(in crate::actor::native::spawn) identity: SpawnIdentity,
    pub(in crate::actor::native::spawn) sender: mpsc::Sender<Envelope>,
    pub(in crate::actor::native::spawn) transport: Arc<NativeBinding>,
    pub(in crate::actor::native::spawn) slots: Box<ActorSlots>,
    pub(in crate::actor::native::spawn) state: A::State,
    pub(in crate::actor::native::spawn) after_init: Vec<Envelope>,
}

impl Spawner {
    /// Resolve identity and perform the current namespace and tombstone
    /// preflight before actor construction. Named-subname and typed-parent
    /// gates run in `SpawnBuilder` before this can allocate a counter.
    pub(in crate::actor::native::spawn) fn prepare_identity<A>(
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
        let (parent_mailbox, carry, full_name) = parent.map_or_else(
            || (MailboxId::NONE, child_actor.0, Arc::from(format!("{}:{}", A::NAMESPACE, subname))),
            |parent| {
                let carry = fold_lineage(parent.carry(), child_actor);
                let name: Arc<str> = Arc::from(format!("{}/{}:{}", parent.canonical_name(), A::NAMESPACE, subname));
                (parent.mailbox(), carry, name)
            },
        );
        let id = MailboxId(with_tag(Tag::Mailbox, carry));
        Ok(SpawnIdentity { id, parent: parent_mailbox, carry, canonical_name: full_name, subname })
    }

    /// Legacy eager preflight. Handler staging deliberately uses only
    /// [`Self::prepare_identity`]; namespace ownership and liveness are global
    /// facts and therefore move to owner-time activation reservation.
    pub(in crate::actor::native::spawn) fn preflight<A>(
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
    pub(in crate::actor::native::spawn) fn build<A>(
        self: &Arc<Self>,
        identity: SpawnIdentity,
        config: A::Config,
        params: A::Params,
        after_init: Vec<Envelope>,
    ) -> Result<StagedActor<A>, SpawnError>
    where
        A: Instanced + NativeActor,
    {
        let SpawnIdentity { id, parent, carry, canonical_name, subname } = identity;

        // Construct + init on caller's thread. Build the inbox pair
        // up-front so init may publish its self-id (`NativeInitCtx::self_id`
        // reads the binding's `self_mailbox`, which is this folded `id`);
        // the spawn thread doesn't exist yet.
        let (tx, rx) = mpsc::channel::<Envelope>();

        let transport = Arc::new(NativeBinding::new_with_parent::<A>(
            Arc::clone(&self.mailer),
            id,
            parent,
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
            identity: SpawnIdentity { id, parent, carry, canonical_name, subname },
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
    /// [`SpawnOutcome`](crate::actor::native::spawn::SpawnOutcome) (a parent actor's `TaskDone`, or the channel a
    /// post-seal external caller is blocked on). `None` is for fixtures that
    /// exercise the owner path with nothing waiting on the answer.
    ///
    /// `chain` is the staging site's ADR-0168 §3 declaration of what orders
    /// this birth's effects. It rides to the activation home so the newborn's
    /// `wire` hook can hold whatever chain it names (ADR-0168 §1).
    pub(in crate::actor::native::spawn) fn prepare_commit<A>(
        self: &Arc<Self>,
        staged: StagedActor<A>,
        finalizer: Option<Arc<NativeSpawnFinalizer>>,
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
                )
            })
            .collect();
        let activation =
            LegacyPreparedActivation::<A>::new(Arc::clone(self), id, subname, sender, transport, slots, state, chain);
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
}
