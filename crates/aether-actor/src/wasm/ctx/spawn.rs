//! Child creation — [`ActorTypeTag`] and [`SpawnError`], the [`WasmCtx`]
//! verbs that spawn an inline child (ADR-0114) and tear one down, and the
//! subname resolution plus `init`-and-insert core the spawn paths share.

use aether_data::{ActorId, Kind, MailboxId};

use super::{InlineChild, NO_INBOUND_SOURCE, WasmCtx, WasmInitCtx};
use crate::model::Anyone;
use crate::model::ctx::Erased;
use crate::model::ctx::reply_mode::{ReplyMode, Unchecked};
use crate::model::{Addressable, Instanced, NamespaceError, Subname, validate_namespace_segment};
use crate::reference::ErasedActorRef;
use crate::wasm::bridge::mail;
use crate::wasm::decode::decode_config;
use crate::wasm::inline::{Child, ChildRecord, Registry, Reinserted, unwire_child, wire_seated};
use crate::wasm::{__validate_inline_child_alias, ActorInitError, ErasedWasmActor, Spawns, WasmActor};
use alloc::boxed::Box;
use alloc::string::String;

/// A runtime selector for one of a module's `export!`ed actor types — the
/// `hash(NAMESPACE)` folded id [`WasmCtx::spawn_inline_child_by_tag`]
/// resolves against the module's exported set (issue 2692), and the same
/// tag the ADR-0114 §5 reconstruct arm matches a persisted inline child on.
///
/// A newtype rather than a bare `u64` on purpose: a consumer selects a type
/// with `ActorTypeTag::of::<SomeActor>()` and never names a namespace hash.
/// The tag is `ActorId::singleton(NAMESPACE)`, the actor type's identity, and
/// it reads as an actor-type selector, distinct from a [`MailboxId`] even
/// though the underlying value coincides with the type's depth-1 folded id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActorTypeTag(pub u64);

impl ActorTypeTag {
    /// The actor-type tag for `A` — `ActorId::singleton(A::NAMESPACE)`, the
    /// actor type's identity, folded at compile time
    /// (`Addressable::NAMESPACE` is a `const`).
    #[must_use]
    pub const fn of<A: Addressable>() -> Self {
        Self(ActorId::singleton(A::NAMESPACE).0)
    }
}

/// Why a synchronous spawn verb failed.
///
/// The typed spawn verbs validate the ctx's registry-derived parent actor
/// identity before doing spawn work. An inline child's `init`
/// ([`WasmCtx::spawn_inline_child`], ADR-0114) runs in-process,
/// synchronously, so its failure is reported here as
/// [`SpawnError::InitFailed`].
#[derive(Debug, Clone)]
pub enum SpawnError {
    /// The host returned the zero id while allocating an inline child alias. The alias is a required first-class address, so spawning stops
    /// before configuration decode, initialization, or registry insertion.
    ///
    /// Among the host's refusals is a spent name: an inline child ends by
    /// closing and its name tombstones (ADR-0241 §8), so a key whose child
    /// was despawned, or whose parent closed, is never spawned again.
    AliasAllocationFailed,
    /// The child's type declares a dependency (`#[actor(depends(R))]`,
    /// ADR-0230) with no `Live` route, so the host refused the spawn before
    /// allocating its alias. Dependencies are checked where an actor stands
    /// up (ADR-0241 §4), and for an inline child that is its spawn: the
    /// module loaded, but this child is not built until what it depends on
    /// is live. The host's warning names the child and the missing
    /// namespace.
    DependencyNotLive,
    /// The ctx's mailbox did not identify either the constructed entry actor
    /// or a registered inline actor, so its logical parent type could not be
    /// validated before spawning.
    ParentIdentityUnavailable(MailboxId),
    /// The typed spawn named parent `expected`, but the ctx is executing the
    /// logically distinct actor type `actual`.
    ParentIdentityMismatch { expected: ActorTypeTag, actual: ActorTypeTag },
    /// A by-tag spawn selected an exported actor whose generated cardinality
    /// is not [`Instanced`].
    ActorNotInstanced(ActorTypeTag),
    /// A by-tag spawn selected an instanced actor whose `child_of(..)` list
    /// does not name `parent`.
    PlacementDenied { parent: ActorTypeTag, child: ActorTypeTag },
    /// A [`Subname::Named`] discriminator failed
    /// [`validate_namespace_segment`].
    SubnameInvalid(NamespaceError),
    /// ADR-0114: an inline child's synchronous `init` returned `Err`. The
    /// wrapped [`ActorInitError`] carries the actor's own failure message.
    /// An inline child's `init` runs in-guest during
    /// [`WasmCtx::spawn_inline_child`], so the boot failure comes back
    /// through this `Result`.
    InitFailed(ActorInitError),
    /// An inline child's `wire` returned `Err` (ADR-0247 rule 3). The child
    /// ran its `unwire`, its slot is gone and its alias is retired, as a
    /// despawn leaves it, so its name is spent (ADR-0241 §8). The wrapped
    /// [`ActorInitError`] carries the child's own failure message.
    WireFailed(ActorInitError),
    /// Issue 2692: [`WasmCtx::spawn_inline_child_by_tag`] was handed an
    /// [`ActorTypeTag`] that matched none of the module's `export!`ed actor
    /// types (a stale spec, a script, a tag for a type dropped from the
    /// module). The tag is runtime data, so an unresolvable one is a runtime
    /// error the spawner recovers from rather than a panic — and no host
    /// alias is allocated for it (the export-set fall-through precedes
    /// allocation).
    UnknownActorTag(ActorTypeTag),
}

impl<A, S, M: ReplyMode> WasmCtx<'_, A, S, M> {
    /// ADR-0114: spawn an **inline child** — a co-located child actor that
    /// shares this component's WASM instance, slot, and run-token, while
    /// being addressed and mailed like any actor. `C` is a
    /// `Subname`-discriminated `Instanced` type whose `child_of(..)` lists
    /// `P`.
    ///
    /// The host folds the child's alias [`MailboxId`]
    /// (`{parent}/<C::NAMESPACE>:<subname>`, ADR-0241 §6) and registers a route to
    /// this trampoline's own slot; the SDK then runs `A::init`
    /// **synchronously** and inserts the boxed child into
    /// this ctx's per-component [`Registry`] keyed by the alias. Mail
    /// addressed to the alias lands in this slot and the `export!`
    /// membrane demuxes it to the child; the child's own sends stamp the
    /// child's address as origin and its replies route back.
    ///
    /// A [`Subname::Named`] that fails validation returns
    /// [`SpawnError::SubnameInvalid`]; a `C` that declares a dependency with
    /// no `Live` route returns [`SpawnError::DependencyNotLive`] before `init`;
    /// a synchronous `init` `Err` returns [`SpawnError::InitFailed`].
    ///
    /// A [`Subname::Named`] at which a `C` already stands beneath this actor
    /// answers that child (ADR-0249 §5): nothing is initialised again,
    /// `config` is ignored, and no host call is made. A standing child that
    /// never wired wires now, before the answer, and its failure comes back
    /// as [`SpawnError::WireFailed`]. A `wire` that spawns a child is
    /// therefore safe to run again. A name whose child was despawned is not
    /// standing: it is spent, and the spawn is refused.
    ///
    /// The alias extends the executing actor's lineage, so the same subname
    /// can exist beneath distinct parents in one component cluster. The same
    /// executing id is recorded as the child's logical parent for relative
    /// addressing and replacement reconstruction.
    ///
    /// What comes back is an [`InlineChild<C>`] rather than a bare
    /// [`MailboxId`]: the call already names `C`, so the handle keeps it and
    /// [`InlineChild::send`] checks every subsequent send against `C`'s
    /// handler set. [`InlineChild::erase`] yields the proof a send or
    /// [`Self::despawn_inline_child`] takes, and [`InlineChild::id`] the key
    /// for a registry lookup (a slot table keyed on `MailboxId`).
    ///
    /// `P` must declare `C` in its `#[actor(spawns(..))]` ([`Spawns`]), whose
    /// [`Placement`](Spawns::Placement) proves `C` lists `P` in its
    /// `child_of(..)`, and every `export!` that lists `P` must list `C`,
    /// exported or under `private = [..]`
    /// ([`Rebuildable`](crate::Rebuildable)), so a replace can rebuild it.
    ///
    /// This is the verb for an erased or wire ctx, which does not know its
    /// actor; a ctx typed by its actor uses [`Self::spawn_inline`].
    pub fn spawn_inline_child<P, C>(
        &self,
        subname: Subname<'_>,
        config: &C::Config,
    ) -> Result<InlineChild<C>, SpawnError>
    where
        P: WasmActor + Spawns<C>,
        // `ErasedWasmActor` is the boxing seam every `#[actor]` type emits
        // (ADR-0096) — the registry stores the child as `dyn
        // ErasedWasmActor`, so the bound is the mechanical realisation of
        // "reuse the existing erasure" (no new child-dispatch trait).
        C: Instanced + WasmActor + ErasedWasmActor,
        // iamacoffeepot/aether#2311: `C::init` returns the runtime state, boxed
        // as the erased child (`State = Self` for an un-split component).
        <C as WasmActor>::State: ErasedWasmActor,
    {
        self.validate_spawn_parent::<P>()?;
        self.install_inline::<C>(subname, config)
    }

    /// ADR-0114: spawn an **inline child** naming only the child type — the
    /// typed verb for any spawner whose ctx is typed by its actor, and the
    /// wasm analogue of the native `ctx.spawn_child::<C>` that reads its
    /// parent from the ctx.
    ///
    /// The ctx's actor `A` must declare `C` in its `#[actor(spawns(..))]`
    /// ([`Spawns`]). That one bound carries both proofs the spawn needs: `C`
    /// lists `A` in its `child_of(..)` ([`Placement`](Spawns::Placement)), and
    /// every `export!` that lists `A` must list `C`, exported or under
    /// `private = [..]` ([`Rebuildable`](crate::Rebuildable)), so a replace
    /// can rebuild it. The parent is the ctx's own actor, so there is no `P`
    /// to name or validate at run time, unlike [`Self::spawn_inline_child`],
    /// which an erased or wire ctx uses.
    ///
    /// Everything else — subname resolution, alias allocation, the in-guest
    /// `init`, registry insertion, the child's `wire` — is
    /// [`Self::spawn_inline_child`]'s, and so is the returned
    /// [`InlineChild<C>`]. A ctx whose mailbox identifies no actor still
    /// returns [`SpawnError::ParentIdentityUnavailable`] before any host call:
    /// the parent is read rather than named, not skipped.
    pub fn spawn_inline<C>(&self, subname: Subname<'_>, config: &C::Config) -> Result<InlineChild<C>, SpawnError>
    where
        A: Spawns<C>,
        // The erasure bounds are `spawn_inline_child`'s, for the same reason:
        // the registry stores the child as `dyn ErasedWasmActor`.
        C: Instanced + WasmActor + ErasedWasmActor,
        <C as WasmActor>::State: ErasedWasmActor,
    {
        self.spawn_parent()?;
        self.install_inline::<C>(subname, config)
    }

    /// The spawn body both inline verbs share, entered once their differing
    /// parent-identity check has passed.
    fn install_inline<C>(&self, subname: Subname<'_>, config: &C::Config) -> Result<InlineChild<C>, SpawnError>
    where
        C: Instanced + WasmActor + ErasedWasmActor,
        <C as WasmActor>::State: ErasedWasmActor,
    {
        let (is_counter, full_subname) = resolve_subname(subname)?;
        // The actor-type tag the rehydrate reconstruct matches against the
        // module's exported types (ADR-0114 §5) — the same `hash(NAMESPACE)`
        // tag `init_typed_p32` selects on — and the key the host reads the
        // alias's contract rows by (ADR-0231 §4).
        let type_tag = ActorTypeTag::of::<C>().0;
        if let Some(standing) = self.standing_child(ActorTypeTag(type_tag), is_counter, &full_subname) {
            return self.wire_standing(standing).map(InlineChild::new);
        }
        // A zero alias is the host's refusal, a spent name among them
        // (ADR-0241 §8), and an unmet dependency its own status (ADR-0230):
        // stop before a child is built at an address that never routes.
        let alias =
            __validate_inline_child_alias(mail::spawn_inline_child(self.mailbox, type_tag, is_counter, &full_subname))?;
        // Re-decode an owned `C::Config` for the in-guest `init` from the
        // same bytes the detached path would have shipped — symmetric with
        // `spawn_child`'s encode-in-guest / decode-in-host round-trip, and
        // it sidesteps a `Clone` bound the detached verb also lacks. The
        // decode proves a `ProtocolPath` in the config against the engine's
        // published routes, as the child's own load would (ADR-0231 §3).
        let bytes = config.encode_into_bytes();
        let owned = decode_config::<C::Config>(C::NAMESPACE, &bytes).map_err(SpawnError::InitFailed)?;
        // The executing actor is both the host fold seed and the logical
        // parent recorded for relative addressing and reconstruction.
        let record = ChildRecord { type_tag, full_subname, is_counter, parent: self.mailbox, config_bytes: bytes };
        install_inline_child::<C>(self.inline, alias, record, owned).map(InlineChild::new)
    }

    /// The child already standing where a spawn from this actor would put
    /// one: beneath this actor, as `type_tag`, at the resolved `full_subname`
    /// (ADR-0249 §5). Read from the guest registry, which is the authority on
    /// the aliases this cluster stands at, so it costs no host call. `None`
    /// when no such child stands, and always for a counter spawn, which is a
    /// new name every time.
    fn standing_child(&self, type_tag: ActorTypeTag, is_counter: bool, full_subname: &str) -> Option<MailboxId> {
        if is_counter {
            return None;
        }
        self.inline.resident(MailboxId(self.mailbox), type_tag, full_subname)
    }

    /// Wire the child standing at `standing` when it never wired, and answer
    /// its alias (ADR-0249 §6). A standing child that already wired answers
    /// at once; an unwired one, which a republish rebuilt through `init` and
    /// `on_rehydrate` without `wire`, runs its `wire` now, through a ctx
    /// addressed to its alias. Still no host call and `config` still ignored.
    /// A `wire` that refuses runs its `unwire` and comes back as
    /// [`SpawnError::WireFailed`].
    fn wire_standing(&self, standing: MailboxId) -> Result<MailboxId, SpawnError> {
        match wire_seated(self.inline, standing) {
            Ok(()) => Ok(standing),
            Err(error) => Err(SpawnError::WireFailed(error)),
        }
    }

    /// The actor type this ctx is executing, per the registry — the logical
    /// parent every spawn from here nests under.
    fn spawn_parent(&self) -> Result<ActorTypeTag, SpawnError> {
        self.inline
            .actor_type_tag(MailboxId(self.mailbox))
            .ok_or(SpawnError::ParentIdentityUnavailable(MailboxId(self.mailbox)))
    }

    fn validate_spawn_parent<P: WasmActor>(&self) -> Result<ActorTypeTag, SpawnError> {
        let actual = self.spawn_parent()?;
        let expected = ActorTypeTag::of::<P>();
        if actual != expected {
            return Err(SpawnError::ParentIdentityMismatch { expected, actual });
        }
        Ok(actual)
    }

    /// ADR-0114 / issue 2692: spawn an **inline child** whose type is
    /// selected at runtime by an [`ActorTypeTag`] resolved against the
    /// module's `export!`ed actor set, rather than named at compile time
    /// like [`Self::spawn_inline_child`]. The tag-dispatched sibling of the
    /// typed verb: same subname resolution, same first-class alias, same
    /// in-guest `init` and registry insert — the one difference is that the
    /// type is looked up by tag (through the same export-set table the
    /// reconstruct arm walks, ADR-0114 §5) instead of monomorphized. So a
    /// spawner can hold specs carrying tags and stay non-generic over its
    /// children, dropping the per-child-type generic / hand-written dispatch
    /// a typed spawner would otherwise need.
    ///
    /// `config_bytes` are the selected type's `Config` encoded to its wire
    /// shape (empty for a `Config = ()` type); the resolver decodes them for
    /// the child's `init`, the runtime-data mirror of the typed verb's
    /// in-guest `encode` / `decode` round-trip.
    ///
    /// The `Ok` is the child as an [`ErasedActorRef`]: the host has just
    /// registered the alias, which is the proof (ADR-0230 §3). It is the
    /// by-tag equivalent of the typed verb's
    /// [`InlineChild::erase`] — a spawner that stays non-generic over its
    /// children keeps proofs of them, not positions.
    ///
    /// A [`Subname::Named`] that fails validation returns
    /// [`SpawnError::SubnameInvalid`] before any type lookup. A valid one at
    /// which a child of this `tag` already stands beneath this actor answers
    /// that child before the resolver runs, as the typed verb does: nothing
    /// is initialised again and `config_bytes` are ignored. A standing child
    /// that never wired wires now, before the answer, and its failure comes
    /// back as [`SpawnError::WireFailed`]. The generated
    /// resolver rejects an unknown tag, a non-instanced actor, an unavailable
    /// parent identity, or denied placement before allocating a host alias. A
    /// type that declares a dependency with no `Live` route returns
    /// [`SpawnError::DependencyNotLive`] before `init`. A synchronous `init`
    /// `Err` or a `Config` decode miss returns [`SpawnError::InitFailed`].
    pub fn spawn_inline_child_by_tag(
        &self,
        tag: ActorTypeTag,
        subname: Subname<'_>,
        config_bytes: &[u8],
    ) -> Result<ErasedActorRef, SpawnError> {
        let (is_counter, full_subname) = resolve_subname(subname)?;
        if let Some(standing) = self.standing_child(tag, is_counter, &full_subname) {
            return self.wire_standing(standing).map(ErasedActorRef::new);
        }
        // The resolver is installed on the module's registry by every
        // `export!` init shim — it enumerates the exported type set the
        // lookup needs, which is knowable only inside the macro expansion,
        // so it cannot be a stored SDK-side generic. A registry with no
        // resolver is a raw host-unit registry never wired by `export!`
        // (the seam the host unit tests drive with a synthetic resolver); a
        // real module always installs one at init, before any handler or
        // `wire` runs.
        let Some(resolver) = self.inline.spawn_resolver() else {
            return Err(SpawnError::UnknownActorTag(tag));
        };
        resolver(self.inline, self.mailbox, tag, is_counter, &full_subname, config_bytes).map(ErasedActorRef::new)
    }

    /// ADR-0114: tear down an **inline child** spawned by
    /// [`Self::spawn_inline_child`]. Drops the child from this ctx's
    /// per-component [`Registry`] (running the child's `Drop`), so it
    /// stops handling mail. `child` is the proof the spawn handed back —
    /// [`InlineChild::erase`], [`Self::spawn_inline_child_by_tag`]'s `Ok`, or
    /// `ctx.sender()` when the child mailed this actor. Returns `true` if a
    /// resident child was removed, `false` if the alias named no inline
    /// child — idempotent, so despawning an absent or already-gone alias is a
    /// clean `false`, not an error.
    ///
    /// **The substrate alias route is retired too** (#4228): the address
    /// departs with the actor it named. The host retires the route and fans
    /// one [`MonitorNotice`](aether_kinds::MonitorNotice) out per watcher, so
    /// a cap holding rows keyed on the child's stamped identity (ADR-0114 §4)
    /// reclaims them — the despawn counterpart of what a close already does
    /// for a departing cluster.
    ///
    /// **The child's name is spent** (ADR-0241 §8): a despawned child closes
    /// and its name tombstones, so a later monitor of it is answered with its
    /// notice at once and spawning the same key beneath the same parent fails
    /// with
    /// [`SpawnError::AliasAllocationFailed`].
    ///
    /// Later mail to a retired alias resolves as *dropped* rather than
    /// resolving to this component's slot: the substrate warns and discards
    /// it, balancing the send so the causal chain still settles (ADR-0080 §2)
    /// rather than leaking. A sender therefore gets an honest "this address
    /// was registered and is gone" instead of mail that silently lands in a
    /// parent that never claimed it.
    ///
    /// The retirement is staged, not immediate — it lands through the
    /// registry owner just after this guest call returns, the same path the
    /// spawn-side publication takes. The child's own `unwire`, which runs
    /// below, therefore still sends through a live alias.
    ///
    /// Callable from any depth: a parent on a child, a sibling on a
    /// sibling, or a child on itself.
    ///
    /// The teardown mirror of the spawn-time `wire` (issue 2746): a child
    /// that wired runs its `unwire` before it is dropped, once. A seated
    /// child runs it here. A child despawning itself is out of its slot
    /// while its handler runs, so nothing is seated to unwire here and only
    /// the slot is removed; the caller that holds the child runs its `unwire`
    /// when the handler returns, and then drops it (ADR-0249 §4). A child
    /// whose `unwire` has already run, at its parent's close, runs none.
    // Despawn is a command; its `bool` ("was a resident child removed")
    // is informational and may be ignored, the same contract as
    // `BTreeMap::remove` / `HashSet::remove` (neither is `#[must_use]`).
    // The pedantic candidate lint only fires now that the body reads a
    // borrowed registry rather than mutating a crate-global static.
    #[allow(clippy::must_use_candidate)]
    pub fn despawn_inline_child(&self, child: ErasedActorRef) -> bool {
        let child = child.id();
        if let Some(seated) = self.inline.take(child) {
            drop(unwire_child(self.inline, child, seated));
        }
        remove_and_retire(self.inline, child)
    }
}

/// Remove the slot at `alias` and, when one stood there, retire the alias's
/// substrate route. Answers whether a slot stood.
///
/// Only a slot that was actually ours earns a retirement: the guest registry
/// is the authority on which aliases this cluster resides at, so an
/// idempotent re-despawn (or an alias that named no child) leaves the
/// substrate untouched. The retirement is wasm32-only: the host build carries
/// no FFI surface, and its inline registry has no substrate route behind it.
fn remove_and_retire(registry: &Registry, alias: MailboxId) -> bool {
    let removed = registry.remove(alias);
    #[cfg(target_family = "wasm")]
    if removed {
        mail::despawn_inline_child(alias.0);
    }
    removed
}

/// Resolve a [`Subname`] into the `(is_counter, discriminator)` pair the
/// inline spawn host fns take, used by [`WasmCtx::spawn_inline_child`].
/// `Counter` passes an empty discriminator
/// the host ignores (it assigns a bare monotonic counter and produces just
/// `n.to_string()`); `Named` validates the caller-supplied segment (no `:`,
/// no control/whitespace, not empty) then passes it bare as the flat
/// discriminator — convention: no `.` in a discriminator.
fn resolve_subname(subname: Subname<'_>) -> Result<(bool, String), SpawnError> {
    match subname {
        Subname::Counter => Ok((true, String::new())),
        Subname::Named(name) => {
            validate_namespace_segment(name).map_err(SpawnError::SubnameInvalid)?;
            Ok((false, String::from(name)))
        }
    }
}

/// Build an inline child's actor value, wire it, and seat it under its alias
/// in `registry` (ADR-0114). Split out of [`WasmCtx::spawn_inline_child`] so
/// the in-guest `init` + registry insert is exercisable on the host build
/// (where the `spawn_inline_child` host fn is a panicking stub): the unit
/// test calls this with a local registry, a synthetic alias, and an owned
/// config.
///
/// ADR-0114 §5: `type_tag` / `full_subname` / `is_counter` are recorded in
/// the slot so a `replace_component` swap can reconstruct the child by
/// type and re-fold its metadata. `config_bytes` (issue 2690) is the
/// child's encoded `Config` — the same bytes `config` was decoded from —
/// retained in the slot so a subsequent dehydrate/reconstruct cycle can
/// re-init the child from its real config instead of empty bytes.
///
/// `pub(crate)` so the by-tag spawn core
/// ([`crate::wasm::inline::compose::spawn_one_child`], issue 2692) shares
/// this exact `init` + insert step with the typed verb rather than
/// copying it.
///
/// The child's slot is reserved once `init` has returned, and the fresh
/// child stays on this call's stack while its `wire` runs (issue 2746)
/// through a [`WasmCtx`] addressed to its alias. Its slot is out for that
/// time, as a dispatched child's is, so a `wire` that spawns a nested inline
/// child re-enters the registry without aliasing its interior-mutable map
/// and finds its spawner's slot. Once `wire` has returned `Ok` the child is
/// seated wired. Only the two fresh-spawn paths funnel here; the
/// `replace_component` reconstruct path (`reconstruct_one_child`) has its
/// own insert and runs `init` + `on_rehydrate`, not `wire`: a rebuild
/// itself never wires, and the `wire` export wires rebuilt children after
/// the entry actor's `wire` (ADR-0249 §6).
///
/// A child that despawned itself inside its own `wire` has no slot to be
/// seated in. Its `wire` returned `Ok`, so it runs its `unwire` and drops,
/// as a child that despawns itself in a handler does (ADR-0249 §4); the
/// spawn still answers the alias, which is now spent.
///
/// A `wire` that returns `Err` fails the spawn (ADR-0247 rule 3). The hook
/// was entered, so the child runs its `unwire`; then its slot is removed and
/// its alias retired, the three things [`WasmCtx::despawn_inline_child`]
/// does, and the error comes back as [`SpawnError::WireFailed`]. The alias
/// is retired only when the slot still stood: a child that despawned itself
/// before its `wire` failed has retired it already.
pub fn install_inline_child<A>(
    registry: &Registry,
    alias: MailboxId,
    record: ChildRecord,
    config: A::Config,
) -> Result<MailboxId, SpawnError>
where
    A: WasmActor + ErasedWasmActor,
    // iamacoffeepot/aether#2311: `A::init` returns the runtime state, boxed as
    // the erased child. For an un-split component `State = Self`.
    <A as WasmActor>::State: ErasedWasmActor,
{
    let mut ctx = WasmInitCtx::__new();
    // ADR-0156 §2: inline children resolve `Params` to the compiled default
    // (empty params for now), mirroring the `()`-config round-trip.
    let params = <A::Params as Default>::default();
    let mut fresh: Box<dyn ErasedWasmActor> =
        Box::new(A::init(config, params, &mut ctx).map_err(SpawnError::InitFailed)?);
    registry.reserve(alias, record);

    let mut wire_ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(alias.0, registry, NO_INBOUND_SOURCE);
    if let Err(error) = fresh.erased_wire(&mut wire_ctx) {
        fresh.erased_unwire(&mut wire_ctx);
        drop(fresh);
        remove_and_retire(registry, alias);
        return Err(SpawnError::WireFailed(error));
    }

    if let Reinserted::Departed(departed) = registry.reinsert(alias, Child::Wired(fresh)) {
        drop(unwire_child(registry, alias, departed));
    }
    Ok(alias)
}
