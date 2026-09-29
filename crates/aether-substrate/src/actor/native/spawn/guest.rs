//! Staging the birth of a published guest (ADR-0241 §5, §6).
//!
//! A guest runs in a native host actor `H`, but it is named by what it is:
//! the namespace its module publishes, never `H::NAMESPACE`. So the birth
//! resolves its own identity (`NS`, `NS:key`, or `parent/NS:key`), holds no
//! native namespace, and the registry owner admits it only where the
//! publication table binds that namespace to the birth's module (§3). Its
//! completion carries a [`ProtocolRef<P>`], the rows of `H` the host
//! controls the guest through, since the name carries no type identity an
//! `ActorRef<H>` could claim.
//!
//! The staging after identity is the native handler builder's: a
//! [`PreparedBirth`] armed as a task that owes no reply (ADR-0243 §9).

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use aether_actor::{ActorRef, CoveredBy, ErasedActorRef, Instanced, Protocol, ProtocolRef};
use aether_data::{ActorId, BlobHash, ErasedActorPath, Kind};

use crate::actor::native::NativeActor;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::mail::MailId;
use crate::runtime::effect_chain::{EffectChain, Uncaused};

use super::activation::BirthOutcome;
use super::reservation::ChildReservationKey;
use super::staged::PreparedBirth;
use super::{SpawnError, SpawnReceipt, Spawner, Subname};

/// Where and as what a published guest is born (ADR-0241 §5).
///
/// | `key`     | `parent`  | name            |
/// |-----------|-----------|-----------------|
/// | `None`    | `None`    | `NS`            |
/// | `Some(k)` | `None`    | `NS:k`          |
/// | `Some(k)` | `Some(p)` | `p/NS:k`        |
/// | `None`    | `Some(_)` | refused: [`SpawnError::GuestPlacement`] |
pub struct GuestBirth<'a> {
    /// The published name: `NS`, or `NS.<hash>` for a content-addressed
    /// module (ADR-0241 §3).
    pub namespace: &'a str,
    /// The module that must hold `namespace` in the publication table.
    pub module: BlobHash,
    /// `None` for a singleton; [`Subname::Counter`] draws the spawner's
    /// counter, as a native birth's does.
    pub key: Option<Subname<'a>>,
    /// The live actor the guest is born beneath, or `None` for a root.
    pub parent: Option<ErasedActorRef>,
}

/// The authoritative fate of one staged guest birth, delivered through the
/// ADR-0093 task completion path once the registry owner has decided it.
///
/// Self-identifying on both arms, as [`SpawnOutcome`](super::SpawnOutcome)
/// is. The `Ok` arm is a [`ProtocolRef<P>`]: the host type's reference,
/// minted once the route is `Live` and narrowed to the protocol its host
/// rows cover before it leaves the finalizer, so no reference claims the
/// guest is the host type.
pub struct GuestOutcome<P> {
    pub canonical_name: ErasedActorPath,
    pub result: Result<ProtocolRef<P>, SpawnError>,
}

impl<H: 'static, P: CoveredBy<H> + 'static> BirthOutcome<H> for GuestOutcome<P> {
    fn decided(canonical_name: ErasedActorPath, result: Result<ActorRef<H>, SpawnError>) -> Self {
        Self { canonical_name, result: result.map(ActorRef::narrow) }
    }
}

// By hand, because a derive would bound `P: Debug` and protocol types do not
// implement it.
impl<P> fmt::Debug for GuestOutcome<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuestOutcome")
            .field("canonical_name", &self.canonical_name)
            .field("result", &self.result)
            .finish()
    }
}

/// Handler-owned guest builder, the surface
/// [`NativeCtx::spawn_guest`](crate::actor::native::ctx::NativeCtx::spawn_guest)
/// hands back. Its one terminal, [`stage_with`](Self::stage_with), performs
/// only local preparation during the actor turn and appends one ordered
/// prepared birth to the staging actor's binding (ADR-0165).
pub struct GuestSpawnBuilder<'ctx, H: Instanced + NativeActor, P> {
    spawner: Arc<Spawner>,
    birth: GuestBirth<'ctx>,
    /// The runtime identity `birth.parent` proves, read off the proof by the
    /// staging ctx.
    parent: Option<ActorRuntimeIdentity>,
    config: H::Config,
    params: H::Params,
    parent_binding: Arc<NativeBinding>,
    completion_root: Option<MailId>,
    _protocol: PhantomData<fn() -> P>,
}

impl<'ctx, H, P> GuestSpawnBuilder<'ctx, H, P>
where
    H: Instanced + NativeActor,
    P: Protocol + CoveredBy<H> + 'static,
{
    pub(crate) fn new(
        spawner: Arc<Spawner>,
        birth: GuestBirth<'ctx>,
        parent: Option<ActorRuntimeIdentity>,
        config: H::Config,
        params: H::Params,
        parent_binding: Arc<NativeBinding>,
        completion_root: Option<MailId>,
    ) -> Self {
        Self { spawner, birth, parent, config, params, parent_binding, completion_root, _protocol: PhantomData }
    }

    /// Prepare and stage the guest birth with a completion context
    /// (ADR-0243 §9). The birth owes no reply. `context` is stored in the
    /// request-context table under the birth's request id, and the
    /// `#[handler(task)]` completion takes it with `ctx.take_context::<C>()`.
    /// The authoritative result lands as `TaskDone<GuestOutcome<P>>`.
    ///
    /// # Errors
    ///
    /// The [`SpawnError`] of the first synchronous step that refuses the
    /// birth, with `context` handed back unstored: a keyless child
    /// ([`SpawnError::GuestPlacement`]), an invalid namespace or key, a
    /// path over the scope caps, a name the parent already holds, or a
    /// failed build.
    pub fn stage_with<C: Kind>(self, context: C) -> Result<SpawnReceipt, (SpawnError, C)> {
        let birth = match self.prepare() {
            Ok(birth) => birth,
            Err(error) => return Err((error, context)),
        };
        let request = birth.mint_request();
        birth.parent_binding.store_request_context(request, context);
        Ok(birth.stage_as_task(request))
    }

    /// Run every fallible step — identity, the parent-local reservation for
    /// a parented birth, and the build — with nothing armed or stored yet,
    /// so a refusal leaves no trace.
    fn prepare(self) -> Result<PreparedBirth<H, GuestOutcome<P>>, SpawnError> {
        let Self { spawner, birth, parent, config, params, parent_binding, completion_root, .. } = self;
        let GuestBirth { namespace, module, key, .. } = birth;

        let (identity, node) = spawner.prepare_guest_identity(namespace, key, parent.as_ref())?;
        let parent_reservation = match &parent {
            Some(parent) => {
                let key = ChildReservationKey::new(parent.mailbox(), ActorId::singleton(namespace), node);
                let reservation = parent_binding
                    .reserve_child(key)
                    .ok_or_else(|| SpawnError::SubnameInUse { full_name: identity.canonical_name.to_string() })?;
                Some(reservation)
            }
            None => None,
        };
        let staged = spawner.build::<H>(identity, config, params, Vec::new())?;
        let chain = completion_root.map_or(EffectChain::Uncaused(Uncaused::ChainlessTurn), EffectChain::Held);
        Ok(PreparedBirth {
            spawner,
            parent_binding,
            completion_root,
            chain,
            parent_reservation,
            staged,
            guest: Some((Arc::from(namespace), module)),
            _outcome: PhantomData,
        })
    }
}
