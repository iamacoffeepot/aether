//! Spawning a native type by mail from its boot-time publication (ADR-0241
//! §9).
//!
//! A native type is mail-spawnable when its `Params` is `()`: nothing but
//! its namespace, a key, and a parent is needed to stand one up, and its
//! `Config` resolves from the engine's own source stack (ADR-0090,
//! ADR-0156 §5), never from mail bytes. The native `#[actor]` runtime
//! expansion submits one [`NativeSpawnEntry`] per non-generic type whose
//! `Params` is `()`, singleton or instanced, and the component host's spawn
//! door looks it up by namespace.
//!
//! An entry does three things for its type. It says whether the type is
//! instanced and whether it declares `root`. It proves the `Live` route at
//! the name a spawn would take as [`SpawnDelivery`], the framework row every
//! native actor serves. And, for an instanced type, it stages the birth
//! through the same prepared-birth path `spawn_child` and `spawn_guest` take,
//! so the birth runs the ordinary owner commit, activation, and publication
//! table hold (ADR-0241 §3), completing with a [`NativeSpawnOutcome`].

use std::any::type_name;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use aether_actor::{ActorRef, Addressable, ErasedActorRef, Lifecycle, Many, One, Protocol, ProtocolRef, Row};
use aether_data::name_inventory::{child_entries, inventory};
use aether_data::{ActorId, ErasedActorPath, RequestId, Tag, with_tag};
use aether_kinds::{ActorSpawnDelivered, SpawnResult};

use crate::actor::native::NativeActor;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::chassis::error::BootError;
use crate::config::ConfigMember;
use crate::mail::registry::Registry;
use crate::mail::{MailId, MailboxId};
use crate::runtime::effect_chain::{EffectChain, Uncaused};

use super::activation::BirthOutcome;
use super::reservation::ChildReservationKey;
use super::staged::PreparedBirth;
use super::{SpawnError, SpawnReceipt, Spawner, Subname};

/// The one framework row every native actor serves (ADR-0241 §9):
/// `aether.actor.spawn_delivered`, answered with a [`SpawnResult`] in the
/// receiving actor's own name. A native spawn by mail completes with a
/// [`ProtocolRef<SpawnDelivery>`], and the component host hands the spawn's
/// held reply off through it, so the requester hears from the instance.
pub struct SpawnDelivery;

impl Protocol for SpawnDelivery {
    type Rows = (Row<ActorSpawnDelivered, SpawnResult>,);
}

/// The authoritative fate of one native birth by mail, delivered through the
/// ADR-0093 task completion path once the registry owner has decided it.
/// Its `Ok` arm is the born actor's [`SpawnDelivery`] proof, the one row the
/// spawn's reply is handed off through, since the host that staged the birth
/// does not name its type.
pub struct NativeSpawnOutcome {
    pub canonical_name: ErasedActorPath,
    pub result: Result<ProtocolRef<SpawnDelivery>, SpawnError>,
}

impl<A: NativeActor> BirthOutcome<A> for NativeSpawnOutcome {
    fn decided(canonical_name: ErasedActorPath, result: Result<ActorRef<A>, SpawnError>) -> Self {
        Self { canonical_name, result: result.map(Registry::spawn_delivery) }
    }
}

impl fmt::Debug for NativeSpawnOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeSpawnOutcome")
            .field("canonical_name", &self.canonical_name)
            .field("result", &self.result)
            .finish()
    }
}

/// One mail-spawnable native type, collected at link time: the native
/// `#[actor]` runtime expansion submits one for every non-generic type whose
/// `Params` is `()` (ADR-0241 §9). Built only by [`Self::of`], so every fact
/// it carries is read off the type it names.
pub struct NativeSpawnEntry {
    namespace: &'static str,
    type_name: fn() -> &'static str,
    instanced: bool,
    declares_root: fn() -> bool,
    live: LiveLookup,
    stager: fn() -> Option<PrepareBirth>,
}

inventory::collect!(NativeSpawnEntry);

/// Proves the `Live` route at the name the entry's type takes for a key
/// beneath a parent.
type LiveLookup = fn(&Registry, Option<&str>, Option<ErasedActorRef>) -> Option<ProtocolRef<SpawnDelivery>>;

/// Stages one instanced birth of the entry's type.
#[doc(hidden)]
pub type PrepareBirth = for<'a> fn(NativeBirthSite<'a>) -> Result<Box<dyn StageableBirth>, SpawnError>;

impl NativeSpawnEntry {
    /// The entry for native type `A`. The `#[actor]` expansion passes two
    /// answers its [`probe`] reads off `A`'s own impls: `declares_root`,
    /// whether `A: Root`, and `stager`, which stages `A`'s birth when `A` is
    /// instanced and its `Config` is a member of the engine's config source
    /// stack, and is `None` otherwise.
    #[doc(hidden)]
    #[must_use]
    pub const fn of<A>(declares_root: fn() -> bool, stager: fn() -> Option<PrepareBirth>) -> Self
    where
        A: NativeActor + Lifecycle<<A as NativeActor>::State, Params = ()>,
        A::Resolver: NativeCardinality,
    {
        Self {
            namespace: A::NAMESPACE,
            type_name: type_name::<A>,
            instanced: <A::Resolver as NativeCardinality>::INSTANCED,
            declares_root,
            live: live::<A>,
            stager,
        }
    }

    /// Every entry declaring `namespace`: none for a type whose `Params` is
    /// not `()`, more than one where several linked types share the
    /// namespace (a chassis composes one of them).
    pub fn declaring(namespace: &str) -> impl Iterator<Item = &'static Self> + '_ {
        inventory::iter::<Self>.into_iter().filter(move |entry| entry.namespace == namespace)
    }

    /// The namespace the type declares.
    #[must_use]
    pub fn namespace(&self) -> &'static str {
        self.namespace
    }

    /// The type's name, for refusals.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        (self.type_name)()
    }

    /// Whether the type is instanced: a singleton is composed at boot and
    /// never born by mail.
    #[must_use]
    pub fn instanced(&self) -> bool {
        self.instanced
    }

    /// Whether a birth of the type can be staged by mail: it is instanced,
    /// and its `Config` resolves from the engine's config source stack. A
    /// type whose `Config` carries the wiring its parent hands it at a
    /// `spawn_child` has nothing a spawn by mail could build it from.
    #[must_use]
    pub fn stageable(&self) -> bool {
        (self.stager)().is_some()
    }

    /// Whether the type declares `root`, so it may be born with no parent.
    #[must_use]
    pub fn declares_root(&self) -> bool {
        (self.declares_root)()
    }

    /// Whether the type declares `child_of` the type named `parent_namespace`,
    /// read from the link-time placement facts its `#[actor]` declaration
    /// submits (ADR-0166).
    #[must_use]
    pub fn declares_child_of(&self, parent_namespace: &str) -> bool {
        child_entries()
            .any(|entry| entry.child_namespace == self.namespace && entry.parent_namespace == parent_namespace)
    }

    pub(crate) fn live(
        &self,
        registry: &Registry,
        key: Option<&str>,
        parent: Option<ErasedActorRef>,
    ) -> Option<ProtocolRef<SpawnDelivery>> {
        (self.live)(registry, key, parent)
    }

    pub(crate) fn stager(&self) -> Option<PrepareBirth> {
        (self.stager)()
    }
}

/// Prove the `Live` route at the name `A` takes for `key` beneath `parent`.
fn live<A>(registry: &Registry, key: Option<&str>, parent: Option<ErasedActorRef>) -> Option<ProtocolRef<SpawnDelivery>>
where
    A: NativeActor,
    A::Resolver: NativeCardinality,
{
    <A::Resolver as NativeCardinality>::position::<A>(key, parent)
        .and_then(|position| registry.live_spawn_delivery(position))
}

/// What one instanced birth by mail is staged from: the staging actor's
/// binding and spawner, its chain, and where the birth is placed.
#[doc(hidden)]
pub struct NativeBirthSite<'a> {
    pub(crate) spawner: Arc<Spawner>,
    pub(crate) binding: Arc<NativeBinding>,
    pub(crate) completion_root: Option<MailId>,
    pub(crate) parent: Option<ActorRuntimeIdentity>,
    pub(crate) key: Subname<'a>,
}

/// A prepared native birth by mail, past every fallible step and armed by
/// nothing yet, with its type erased.
#[doc(hidden)]
pub trait StageableBirth {
    /// Arm the birth's completion as a task under `request` and stage it.
    fn stage_as_task(self: Box<Self>, request: RequestId) -> SpawnReceipt;
}

impl<A: aether_actor::Instanced + NativeActor> StageableBirth for PreparedBirth<A, NativeSpawnOutcome> {
    fn stage_as_task(self: Box<Self>, request: RequestId) -> SpawnReceipt {
        (*self).stage_as_task(request)
    }
}

/// The cardinality half of an entry, dispatched on the type's resolver: a
/// singleton ([`One`]) is found at its namespace and never born by mail; an
/// instanced type ([`Many`]) is found at `NS:key` or `parent/NS:key` and
/// born there.
#[doc(hidden)]
pub trait NativeCardinality: Sized {
    /// Whether the resolver keys its instances.
    const INSTANCED: bool;

    /// The position `A` takes for `key` beneath `parent`, folded exactly as
    /// its birth folds it, or `None` for a placement its cardinality never
    /// takes.
    fn position<A: Addressable<Resolver = Self>>(
        key: Option<&str>,
        parent: Option<ErasedActorRef>,
    ) -> Option<MailboxId>;

    /// The stager of one birth of `A`, or `None` for a singleton.
    fn prepare<A>() -> Option<PrepareBirth>
    where
        A: NativeActor + Addressable<Resolver = Self> + Lifecycle<<A as NativeActor>::State, Params = ()>,
        A::Config: ConfigMember;
}

impl NativeCardinality for One {
    const INSTANCED: bool = false;

    fn position<A: Addressable<Resolver = Self>>(
        key: Option<&str>,
        parent: Option<ErasedActorRef>,
    ) -> Option<MailboxId> {
        (key.is_none() && parent.is_none()).then(|| A::resolve(0, ()))
    }

    fn prepare<A>() -> Option<PrepareBirth>
    where
        A: NativeActor + Addressable<Resolver = Self> + Lifecycle<<A as NativeActor>::State, Params = ()>,
        A::Config: ConfigMember,
    {
        None
    }
}

impl NativeCardinality for Many {
    const INSTANCED: bool = true;

    fn position<A: Addressable<Resolver = Self>>(
        key: Option<&str>,
        parent: Option<ErasedActorRef>,
    ) -> Option<MailboxId> {
        let key = key?;
        // A root birth's carry is its own node (ADR-0099 §3), as
        // `Spawner::prepare_identity` folds it.
        Some(parent.map_or_else(
            || MailboxId(with_tag(Tag::Mailbox, ActorId::instanced(A::NAMESPACE, key).0)),
            |parent| A::resolve(parent.id().0, key),
        ))
    }

    fn prepare<A>() -> Option<PrepareBirth>
    where
        A: NativeActor + Addressable<Resolver = Self> + Lifecycle<<A as NativeActor>::State, Params = ()>,
        A::Config: ConfigMember,
    {
        Some(prepare_instanced::<A>)
    }
}

/// Run every fallible step of one instanced birth of `A` by mail — config
/// resolution, identity, the parent-local reservation a parented birth takes
/// on the staging actor, and the build — with nothing armed or stored yet.
fn prepare_instanced<A>(site: NativeBirthSite<'_>) -> Result<Box<dyn StageableBirth>, SpawnError>
where
    A: aether_actor::Instanced + NativeActor + Lifecycle<<A as NativeActor>::State, Params = ()>,
    A::Config: ConfigMember,
{
    let NativeBirthSite { spawner, binding, completion_root, parent, key } = site;
    let config = spawner
        .config_stack()
        .resolve::<A::Config>()
        .map_err(|error| SpawnError::InitFailed(BootError::from(error)))?;

    let identity = spawner.prepare_identity::<A>(key, parent.as_ref())?;
    let parent_reservation = match &parent {
        Some(parent) => {
            let key = ChildReservationKey::new(
                parent.mailbox(),
                ActorId::singleton(A::NAMESPACE),
                ActorId::instanced(A::NAMESPACE, &identity.subname),
            );
            let reservation = binding
                .reserve_child(key)
                .ok_or_else(|| SpawnError::SubnameInUse { full_name: identity.canonical_name.to_string() })?;
            Some(reservation)
        }
        None => None,
    };
    let staged = spawner.build::<A>(identity, config, (), Vec::new())?;
    let chain = completion_root.map_or(EffectChain::Uncaused(Uncaused::ChainlessTurn), EffectChain::Held);
    Ok(Box::new(PreparedBirth::<A, NativeSpawnOutcome> {
        spawner,
        parent_binding: binding,
        completion_root,
        chain,
        parent_reservation,
        staged,
        guest: None,
        _outcome: PhantomData,
    }))
}

/// The probe the native `#[actor]` expansion reads two facts about `A`
/// through, each from `A`'s own impls: `(&SpawnProbe::<A>::new())` resolves
/// a method to the trait whose impl applies to `SpawnProbe<A>` itself when
/// `A` meets its bounds, and to the fallback implemented one reference
/// further out otherwise, because method resolution tries the by-value
/// receiver before the auto-referenced one.
///
/// - `declares_root()` is `A`'s `Root` impl, the fact the chassis spawn
///   surfaces bound on.
/// - `stager()` stages `A`'s birth when its resolver keys instances and its
///   `Config` is a [`ConfigMember`], and is `None` otherwise.
#[doc(hidden)]
pub mod probe {
    use std::marker::PhantomData;

    use aether_actor::{Lifecycle, Root};

    use super::NativeCardinality;
    pub use super::PrepareBirth;
    use crate::actor::native::NativeActor;
    use crate::config::ConfigMember;

    /// A type-level stand-in for `A`, probed by method resolution.
    pub struct SpawnProbe<A>(PhantomData<fn() -> A>);

    impl<A> SpawnProbe<A> {
        #[must_use]
        pub const fn new() -> Self {
            Self(PhantomData)
        }
    }

    impl<A> Default for SpawnProbe<A> {
        fn default() -> Self {
            Self::new()
        }
    }

    /// The answer for a type that implements `Root`.
    pub trait DeclaresRoot {
        fn declares_root(&self) -> bool {
            true
        }
    }

    impl<A: Root> DeclaresRoot for SpawnProbe<A> {}

    /// The answer for any other type, reached only through one more
    /// reference.
    pub trait DeclaresNoRoot {
        fn declares_root(&self) -> bool {
            false
        }
    }

    impl<A> DeclaresNoRoot for &SpawnProbe<A> {}

    /// The stager of a type whose `Config` the engine's source stack
    /// resolves: its cardinality's, `None` for a singleton.
    pub trait StagesByMail {
        fn stager(&self) -> Option<PrepareBirth>;
    }

    impl<A> StagesByMail for SpawnProbe<A>
    where
        A: NativeActor + Lifecycle<<A as NativeActor>::State, Params = ()>,
        A::Resolver: NativeCardinality,
        A::Config: ConfigMember,
    {
        fn stager(&self) -> Option<PrepareBirth> {
            <A::Resolver as NativeCardinality>::prepare::<A>()
        }
    }

    /// The answer for a type whose `Config` is spawn-time wiring, reached
    /// only through one more reference.
    pub trait StagesNever {
        fn stager(&self) -> Option<PrepareBirth> {
            None
        }
    }

    impl<A> StagesNever for &SpawnProbe<A> {}
}
