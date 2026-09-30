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

use std::marker::PhantomData;
use std::sync::Arc;

use aether_actor::{HandlesKind, Instanced, validate_namespace_segment};
use aether_data::{ActorId, ActorMail, BlobHash, ErasedActorPath, Kind, RequestId};

use crate::actor::native::NativeActor;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::offload::blocking::{DeferredCompletion, IntoDeferredReply};
use crate::actor::native::spawn::activation::{BirthOutcome, NativeSpawnFinalizer, SpawnFinalizer};
use crate::mail::MailId;
use crate::runtime::effect_chain::{EffectChain, OrderingDevice, Uncaused};
use crate::runtime::wire_root::WireRoot;

use super::reservation::{ChildReservationKey, ParentReservation};
use super::spawner::prepare::StagedActor;
use super::{SpawnBuilder, SpawnError, SpawnOutcome, SpawnReceipt, Spawner, Subname};

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
    completion_root: Option<MailId>,
    /// ADR-0168 §3: what orders this birth's effects. Defaults to the
    /// calling handler's chain, which is the answer for every staging site
    /// that runs on a dispatched mail turn, and to
    /// [`Uncaused::ChainlessTurn`] on a turn that dispatches none;
    /// [`Self::ordered_by`] replaces it where a device other than a hold
    /// does the ordering.
    chain: EffectChain,
}

impl<'ctx, A: Instanced + NativeActor> HandlerSpawnBuilder<'ctx, A> {
    pub(crate) fn new(
        inner: SpawnBuilder<'ctx, A>,
        parent_binding: Arc<NativeBinding>,
        completion_root: Option<MailId>,
    ) -> Self {
        let chain = completion_root.map_or(EffectChain::Uncaused(Uncaused::ChainlessTurn), EffectChain::Held);
        Self { inner, parent_binding, completion_root, chain }
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
        K: ActorMail,
    {
        self.inner = self.inner.after_init(mail);
        self
    }

    /// Prepare and stage a birth with no context. It owes no reply: the
    /// authoritative result later lands as `TaskDone<SpawnOutcome<A>>`,
    /// correlated to [`SpawnReceipt::request`] (ADR-0243 §9).
    ///
    /// # Errors
    ///
    /// The [`SpawnError`] of the first synchronous step that refuses the
    /// birth: an invalid subname, a subname this parent already holds, or a
    /// failed identity preflight or build.
    pub fn stage(self) -> Result<SpawnReceipt, SpawnError> {
        let birth = self.prepare()?;
        let request = birth.mint_request();
        Ok(birth.stage_as_task(request))
    }

    /// Prepare and stage a birth with a completion context (ADR-0243 §9).
    /// The birth owes no reply. `context` is stored in the request-context
    /// table under the birth's request id, as `send_with_context` stores a
    /// request's, and the `#[handler(task)]` completion takes it with
    /// `ctx.take_context::<C>()`. The authoritative result lands as
    /// `TaskDone<SpawnOutcome<A>>`, whose `Ok` arm is the child's
    /// `ActorRef<A>`.
    ///
    /// # Errors
    ///
    /// The [`SpawnError`] of the first synchronous step that refuses the
    /// birth, with `context` handed back unstored.
    pub fn stage_with<C: Kind>(self, context: C) -> Result<SpawnReceipt, (SpawnError, C)> {
        let birth = match self.prepare() {
            Ok(birth) => birth,
            Err(error) => return Err((error, context)),
        };
        let request = birth.mint_request();
        birth.parent_binding.store_request_context(request, context);
        Ok(birth.stage_as_task(request))
    }

    /// Stage a successor birth that inherits an already-owed reply — either a
    /// bare [`DeferredReply`](crate::actor::native::DeferredReply) or the
    /// [`TaskDone`](crate::actor::native::TaskDone) a completion handler is already holding,
    /// whose debt rides inside it. Every synchronous validation/build failure
    /// hands `owed` back untouched so the original terminal reply can still be
    /// sent exactly once.
    ///
    /// The completion owes `owed`'s reply, so no request is minted for it;
    /// the birth is named by its canonical path alone. #7008 removes this
    /// terminal.
    #[allow(
        clippy::result_large_err,
        reason = "the cold synchronous rejection returns the move-only owed reply intact beside the precise SpawnError"
    )]
    pub fn continue_from<R, C>(self, owed: R, context: C) -> Result<ErasedActorPath, (SpawnError, R)>
    where
        R: IntoDeferredReply,
        C: Send + 'static,
    {
        let birth = match self.prepare() {
            Ok(birth) => birth,
            Err(error) => return Err((error, owed)),
        };
        // Every fallible step is behind us, so the debt can finally leave the
        // caller's hands: converting is what makes returning it impossible.
        let (hold, reply_to) = owed.into_deferred_reply().into_parts();
        // The inherited debt names the chain that caused this birth — the
        // successor's own ctx no longer holds it, and the newborn's `wire`
        // hook needs it to cover a birth-completing effect (ADR-0168 §1).
        let chain =
            hold.as_ref().map_or(EffectChain::Uncaused(Uncaused::ChainlessTurn), |hold| EffectChain::Held(hold.root()));
        let completion = birth.parent_binding.dispatch_arm::<SpawnOutcome<A>, C>(hold, reply_to, context);
        let canonical_name = birth.staged.identity.canonical_name.clone();
        birth.commit(completion, chain);
        Ok(canonical_name)
    }

    /// Run every fallible step of a birth — subname validation, identity
    /// preflight, the parent-local reservation, and the build — with nothing
    /// armed or stored yet, so a refusal leaves no trace.
    ///
    /// # Panics
    ///
    /// Panics only if internal builder state has already been consumed, which
    /// safe code cannot do because every terminal takes ownership of `self`.
    fn prepare(self) -> Result<PreparedBirth<A>, SpawnError> {
        let Self { inner, parent_binding, completion_root, chain } = self;
        let SpawnBuilder { spawner, subname, config, params, parent, after_init, .. } = inner;
        let config = config.expect("HandlerSpawnBuilder consumed exactly once");
        let params = params.expect("HandlerSpawnBuilder consumed exactly once");
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
        Ok(PreparedBirth {
            spawner,
            parent_binding,
            completion_root,
            chain,
            parent_reservation: Some(parent_reservation),
            staged,
            guest: None,
            _outcome: PhantomData,
        })
    }
}

/// A birth past every fallible step: reserved under its parent when it has
/// one, and built, with no completion armed and nothing staged yet. Both
/// handler builders reach it — the native [`HandlerSpawnBuilder`] and the
/// guest [`GuestSpawnBuilder`](super::GuestSpawnBuilder) — so the staging
/// that follows is one path. `O` is what the birth's completion delivers.
pub(super) struct PreparedBirth<A: Instanced + NativeActor, O = SpawnOutcome<A>> {
    pub(super) spawner: Arc<Spawner>,
    pub(super) parent_binding: Arc<NativeBinding>,
    pub(super) completion_root: Option<MailId>,
    pub(super) chain: EffectChain,
    /// The parent-local key, absent for a root birth: the owner's `Starting`
    /// reservation is a root's uniqueness.
    pub(super) parent_reservation: Option<ParentReservation>,
    pub(super) staged: StagedActor<A>,
    /// A guest birth's published namespace and module (ADR-0241 §3, §6);
    /// `None` for a native birth.
    pub(super) guest: Option<(Arc<str>, BlobHash)>,
    pub(super) _outcome: PhantomData<fn() -> O>,
}

impl<A: Instanced + NativeActor, O: BirthOutcome<A>> PreparedBirth<A, O> {
    /// Mint the birth's request id from the parent's correlation counter,
    /// the one outbound requests and staged tasks draw from (ADR-0243 §9).
    pub(super) fn mint_request(&self) -> RequestId {
        RequestId(self.parent_binding.mint_correlation())
    }

    /// Arm the birth's completion as a staged task that owes no reply,
    /// holding the staging turn's chain, and stage the birth.
    pub(super) fn stage_as_task(self, request: RequestId) -> SpawnReceipt {
        let hold = self.completion_root.map(|root| self.spawner.mailer().acquire_settlement_hold(root));
        let completion = self.parent_binding.dispatch_stage::<O>(hold, request);
        let receipt = SpawnReceipt { canonical_name: self.staged.identity.canonical_name.clone(), request };
        let chain = self.chain;
        self.commit(completion, chain);
        receipt
    }

    /// Hand the armed completion to the birth's finalizer and append the
    /// ordered commit to the parent's outbound work (ADR-0165).
    ///
    /// The birth opens a fresh wire root (ADR-0244 §2) for the sends its
    /// `wire` makes, beside the causing chain `chain` names: the causing
    /// chain keeps every birth-completing effect and every hold, and the
    /// wire root gathers the actor's startup sends under one root a test can
    /// await without the staging caller waiting on them.
    fn commit(self, completion: DeferredCompletion<O>, chain: EffectChain) {
        let Self { spawner, parent_binding, parent_reservation, staged, guest, .. } = self;
        let mailbox_id = staged.identity.id;
        let canonical_name = staged.identity.canonical_name.clone();
        let finalizer: Arc<dyn SpawnFinalizer> = match parent_reservation {
            Some(reservation) => NativeSpawnFinalizer::<A, O>::parented(
                reservation,
                completion,
                mailbox_id,
                canonical_name,
                Arc::downgrade(&staged.transport),
            ),
            None => NativeSpawnFinalizer::<A, O>::rooted(completion, mailbox_id, canonical_name),
        };
        parent_binding.stage_child_birth(spawner.prepare_commit_as(
            staged,
            Some(finalizer),
            chain,
            guest,
            Some(WireRoot::open(spawner.mailer())),
        ));
    }
}
