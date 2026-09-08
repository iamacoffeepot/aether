//! The handler-owned child builder — the only spawn surface
//! [`NativeCtx::spawn_child`](crate::actor::native::ctx::NativeCtx::spawn_child)
//! hands back.
//!
//! Every terminal it carries performs only local preparation during the
//! actor turn and appends one ordered prepared birth to the parent binding,
//! so a handler never takes the spawn path's shared locks mid-turn
//! (ADR-0165). It deliberately exposes no eager terminal: the wrapped
//! [`SpawnBuilder`] is private, so reaching synchronous commit from a
//! handler needs an explicit substrate API change, not a call-site choice.

use std::sync::Arc;

use aether_actor::{HandlesKind, Instanced, validate_namespace_segment};
use aether_data::{ActorId, Kind};

use crate::actor::native::NativeActor;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::offload::blocking::IntoDeferredReply;
use crate::actor::native::spawn::activation::NativeSpawnFinalizer;
use crate::mail::{MailId, Source};
use crate::runtime::effect_chain::{EffectChain, OrderingDevice};
use crate::runtime::trace::SettlementHold;

use super::reservation::ChildReservationKey;
use super::{SpawnBuilder, SpawnError, SpawnReceipt, Subname};

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
            parent.mailbox(),
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
        let finalizer = NativeSpawnFinalizer::parented(
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
            parent.mailbox(),
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
        let finalizer = NativeSpawnFinalizer::parented(
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
