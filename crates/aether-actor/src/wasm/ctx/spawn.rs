//! Child creation — [`ActorTypeTag`] and [`SpawnError`], the [`WasmCtx`]
//! verbs that spawn a detached sibling (ADR-0097) or an inline child
//! (ADR-0114) and tear one down, and the subname resolution plus
//! `init`-and-insert core both spawn paths share.

use aether_data::{Kind, MailboxId, mailbox_id_from_name};

use super::{NO_INBOUND_SOURCE, WasmCtx, WasmInitCtx};
use crate::model::ctx::reply_mode::{Manual, ReplyMode};
use crate::model::{Addressable, ChildOf, Instanced, NamespaceError, Subname, validate_namespace_segment};
use crate::wasm::bridge::mail;
use crate::wasm::inline::Registry;
use crate::wasm::{ActorInitError, ErasedWasmActor, WasmActor};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// A runtime selector for one of a module's `export!`ed actor types — the
/// `hash(NAMESPACE)` folded id [`WasmCtx::spawn_inline_child_by_tag`]
/// resolves against the module's exported set (issue 2692), and the same
/// tag the ADR-0114 §5 reconstruct arm matches a persisted inline child on.
///
/// A newtype rather than a bare `u64` on purpose: it centralizes the single
/// allowed [`mailbox_id_from_name`] call (every other call site is
/// clippy-disallowed) in [`Self::of`], so a consumer selects a type with
/// `ActorTypeTag::of::<SomeActor>()` and never hand-hashes a namespace. It
/// also reads as an actor-type selector, distinct from a [`MailboxId`] even
/// though the underlying hash coincides with the type's depth-1 folded id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActorTypeTag(pub u64);

impl ActorTypeTag {
    /// The actor-type tag for `A` — `hash(A::NAMESPACE)`, folded at compile
    /// time (`Addressable::NAMESPACE` is a `const`). The one sanctioned
    /// [`mailbox_id_from_name`] call outside the id/routing core: it is the
    /// id definition for an actor *type*, so the disallowed-method allow
    /// mirrors [`WasmCtx::spawn_child`] / [`WasmCtx::spawn_inline_child`].
    #[must_use]
    // This is the id definition for an actor type — the single centralized
    // `mailbox_id_from_name` call the by-tag spawn API is built to funnel, so
    // consumers never hand-hash a namespace (all other call sites are
    // clippy-disallowed).
    #[allow(clippy::disallowed_methods)]
    pub const fn of<A: Addressable>() -> Self {
        Self(mailbox_id_from_name(A::NAMESPACE).0)
    }
}

/// Why a synchronous spawn verb failed.
///
/// Both typed spawn verbs validate the ctx's registry-derived parent actor
/// identity before doing spawn work. For detached [`WasmCtx::spawn_child`]
/// (ADR-0097), a later spawn-time failure (a retired / in-use subname, or the
/// sibling's `init` returning `Err`) surfaces asynchronously on the
/// trampoline, not through this `Result`. For the
/// inline [`WasmCtx::spawn_inline_child`] (ADR-0114) the child's `init`
/// runs in-process, synchronously, so its failure is reported here as
/// [`SpawnError::InitFailed`].
#[derive(Debug, Clone)]
pub enum SpawnError {
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
    /// A by-tag spawn selected an instanced actor that declares neither an
    /// exact relationship to `parent` nor module-child composability.
    PlacementDenied { parent: ActorTypeTag, child: ActorTypeTag },
    /// A [`Subname::Named`] discriminator failed
    /// [`validate_namespace_segment`].
    SubnameInvalid(NamespaceError),
    /// ADR-0114: an inline child's synchronous `init` returned `Err`. The
    /// wrapped [`ActorInitError`] carries the actor's own failure message.
    /// Unlike the detached `spawn_child` — whose `init` runs later on the
    /// trampoline and logs asynchronously — an inline child's `init` runs
    /// in-guest during [`WasmCtx::spawn_inline_child`], so the boot failure
    /// comes back through this `Result`.
    InitFailed(ActorInitError),
    /// Issue 2692: [`WasmCtx::spawn_inline_child_by_tag`] was handed an
    /// [`ActorTypeTag`] that matched none of the module's `export!`ed actor
    /// types (a stale spec, a script, a tag for a type dropped from the
    /// module). The tag is runtime data, so an unresolvable one is a runtime
    /// error the spawner recovers from rather than a panic — and no host
    /// alias is allocated for it (the export-set fall-through precedes
    /// allocation).
    UnknownActorTag(ActorTypeTag),
}

impl<M: ReplyMode> WasmCtx<'_, M> {
    /// ADR-0097: spawn a sibling actor type from the same resident
    /// module — the wasm analogue of native `ctx.spawn_child::<P, C>`.
    /// `C` is one of this module's exported `Instanced` types and must
    /// declare `ChildOf<P>`; the SDK verifies that this ctx's actual actor
    /// tag is `P`, resolves `C`'s tag, and encodes `C::Config`. Returns the new
    /// instance's [`MailboxId`] synchronously — it is `hash(name)`
    /// (ADR-0029) — and the instance becomes addressable at
    /// `aether.embedded:<name>`.
    ///
    /// Parent identity and subname validation can `Err` synchronously. A
    /// later spawn-time failure (a retired / in-use subname, or the sibling's
    /// `init` returning `Err`) is logged on the trampoline and does not come
    /// back through this `Result` (ADR-0097 §4). The spawned sibling's Source
    /// is this actor's mailbox, so its replies route here.
    pub fn spawn_child<P, C>(&self, subname: Subname<'_>, config: &C::Config) -> Result<MailboxId, SpawnError>
    where
        P: WasmActor,
        C: ChildOf<P> + Instanced + WasmActor,
    {
        self.validate_spawn_parent::<P>()?;
        // Compile-time actor-type tag for the spawned sibling (hash(NAMESPACE),
        // ADR-0029) — this is the id definition for the new instance, computed
        // before any lineage carry exists.
        let type_tag = ActorTypeTag::of::<C>().0;
        let (is_counter, full_subname) = resolve_subname(subname)?;
        let config_bytes = config.encode_into_bytes();
        let id = mail::spawn_sibling(type_tag, is_counter, &full_subname, &config_bytes);
        Ok(MailboxId(id))
    }

    /// ADR-0114: spawn an **inline child** — a co-located child actor that
    /// shares this component's WASM instance, slot, and run-token, while
    /// being addressed and mailed like any actor. The signature mirrors
    /// [`Self::spawn_child`] (a `Subname`-discriminated `Instanced` type);
    /// the only difference is co-residency.
    ///
    /// The host folds the child's alias [`MailboxId`]
    /// (`{parent}/aether.embedded:<subname>`) and registers a route to
    /// this trampoline's own slot; the SDK then runs `A::init`
    /// **synchronously** (unlike the detached `spawn_child`, whose `init`
    /// runs later on a fresh trampoline) and inserts the boxed child into
    /// this ctx's per-component [`Registry`] keyed by the alias. Mail
    /// addressed to the alias lands in this slot and the `export!`
    /// membrane demuxes it to the child; the child's own sends stamp the
    /// child's address as origin and its replies route back.
    ///
    /// A [`Subname::Named`] that fails validation returns
    /// [`SpawnError::SubnameInvalid`]; a synchronous `init` `Err` returns
    /// [`SpawnError::InitFailed`].
    ///
    /// The alias is folded on the instance carry (flat), so a child's
    /// subname must be unique within the whole cluster — two children that
    /// resolve to the same `aether.embedded:<subname>` collide on one alias.
    /// The spawning actor's real id is recorded as the child's logical
    /// parent so relative addressing (`ctx.parent()` / `ctx.sibling(name)` /
    /// `ctx.child(name)`) resolves over the registry. Per-parent subname
    /// scoping (the nested-alias fold, ADR-0117) is a follow-up needing a
    /// substrate change.
    pub fn spawn_inline_child<P, C>(&self, subname: Subname<'_>, config: &C::Config) -> Result<MailboxId, SpawnError>
    where
        P: WasmActor,
        // `ErasedWasmActor` is the boxing seam every `#[actor]` type emits
        // (ADR-0096) — the registry stores the child as `dyn
        // ErasedWasmActor`, so the bound is the mechanical realisation of
        // "reuse the existing erasure" (no new child-dispatch trait).
        C: ChildOf<P> + Instanced + WasmActor + ErasedWasmActor,
        // iamacoffeepot/aether#2311: `C::init` returns the runtime state, boxed
        // as the erased child (`State = Self` for an un-split component).
        <C as WasmActor>::State: ErasedWasmActor,
    {
        self.validate_spawn_parent::<P>()?;
        let (is_counter, full_subname) = resolve_subname(subname)?;
        let alias = MailboxId(mail::spawn_inline_child(is_counter, &full_subname));
        // Re-decode an owned `C::Config` for the in-guest `init` from the
        // same bytes the detached path would have shipped — symmetric with
        // `spawn_child`'s encode-in-guest / decode-in-host round-trip, and
        // it sidesteps a `Clone` bound the detached verb also lacks.
        let bytes = config.encode_into_bytes();
        let Some(owned) = <C::Config as Kind>::decode_from_bytes(&bytes) else {
            return Err(SpawnError::InitFailed(ActorInitError::new("spawn_inline_child: Config round-trip failed")));
        };
        // The actor-type tag the rehydrate reconstruct matches against the
        // module's exported types (ADR-0114 §5) — the same `hash(NAMESPACE)`
        // tag `init_typed_p32` selects on. This is the id definition for the
        // child type, so the disallowed-method allow mirrors `spawn_child`.
        let type_tag = ActorTypeTag::of::<C>().0;
        // The spawner's real folded id is recorded as the child's logical
        // parent so relative addressing (`ctx.parent()` / `ctx.sibling()`)
        // resolves over the registry. The alias fold itself stays flat on
        // the instance carry (the substrate's current `spawn_inline_child`),
        // so subnames are cluster-unique; per-parent subname scoping (the
        // nested-alias fold) is a follow-up needing a substrate change.
        install_inline_child::<C>(self.inline, alias, type_tag, full_subname, is_counter, self.mailbox, bytes, owned)
    }

    fn validate_spawn_parent<P: WasmActor>(&self) -> Result<ActorTypeTag, SpawnError> {
        let actual = self
            .inline
            .actor_type_tag(MailboxId(self.mailbox))
            .ok_or(SpawnError::ParentIdentityUnavailable(MailboxId(self.mailbox)))?;
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
    /// children, which is what lets the behavior host and the panel drop
    /// their per-child-type generic / hand-written dispatch.
    ///
    /// `config_bytes` are the selected type's `Config` encoded to its wire
    /// shape (empty for a `Config = ()` type); the resolver decodes them for
    /// the child's `init`, the runtime-data mirror of the typed verb's
    /// in-guest `encode` / `decode` round-trip.
    ///
    /// A [`Subname::Named`] that fails validation returns
    /// [`SpawnError::SubnameInvalid`] before any type lookup. The generated
    /// resolver rejects an unknown tag, a non-instanced actor, an unavailable
    /// parent identity, or denied placement before allocating a host alias. A
    /// synchronous `init` `Err` or a `Config` decode miss returns
    /// [`SpawnError::InitFailed`].
    pub fn spawn_inline_child_by_tag(
        &self,
        tag: ActorTypeTag,
        subname: Subname<'_>,
        config_bytes: &[u8],
    ) -> Result<MailboxId, SpawnError> {
        let (is_counter, full_subname) = resolve_subname(subname)?;
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
        resolver(self.inline, self.mailbox, tag, is_counter, &full_subname, config_bytes)
    }

    /// ADR-0114: tear down an **inline child** spawned by
    /// [`Self::spawn_inline_child`]. Drops the child from this ctx's
    /// per-component [`Registry`] (running the child's `Drop`), so it
    /// stops handling mail. `child` is the alias [`MailboxId`] that
    /// `spawn_inline_child` returned (the registry key, the natural
    /// handle). Returns `true` if a resident child was removed, `false` if
    /// the alias named no inline child — idempotent, so despawning an
    /// absent or already-gone alias is a clean `false`, not an error.
    ///
    /// **The substrate alias route is retired too** (#4228): the address
    /// departs with the actor it named. The host retires the route and fans
    /// one [`MonitorNotice`](aether_kinds::MonitorNotice) out per watcher, so
    /// a cap holding rows keyed on the child's stamped identity (ADR-0114 §4)
    /// reclaims them — the despawn counterpart of what a vacate and a close
    /// already do for a departing cluster.
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
    /// sibling, or a child on itself. A self-despawn mid-dispatch drops
    /// correctly — the child is taken out of its slot while it runs, so
    /// `remove` clears the empty slot and the matching `reinsert` on the
    /// inline registry finds nothing and no-ops, dropping the live box at
    /// end of dispatch.
    ///
    /// The teardown mirror of the spawn-time `wire` (issue 2746): a resident
    /// child runs its `unwire` before it is dropped. A self-despawn
    /// mid-dispatch has already taken the box onto the stack, so `take`
    /// finds an empty slot and no `unwire` runs here — the box drops at end
    /// of dispatch via the `reinsert` no-op, and a child unwiring itself
    /// synchronously mid-handler would be the wrong semantic anyway.
    /// Whole-component teardown does not yet cascade `unwire` to resident
    /// inline children (the entry `unwire` FFI runs only the top-level
    /// instance); that is separate future work.
    // Despawn is a command; its `bool` ("was a resident child removed")
    // is informational and may be ignored, the same contract as
    // `BTreeMap::remove` / `HashSet::remove` (neither is `#[must_use]`).
    // The pedantic candidate lint only fires now that the body reads a
    // borrowed registry rather than mutating a crate-global static.
    #[allow(clippy::must_use_candidate)]
    pub fn despawn_inline_child(&self, child: MailboxId) -> bool {
        // Take the resident box onto the stack, run its `unwire` through a
        // ctx addressed to its alias, then drop it; `remove` clears the
        // now-empty slot. A self-despawn (box already taken by dispatch)
        // takes `None`, so `unwire` is skipped and the slot removal stays a
        // clean no-op-then-`false`/`true` per the existing contract.
        if let Some(mut taken) = self.inline.take(child) {
            let mut unwire_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(child.0, self.inline, NO_INBOUND_SOURCE);
            taken.erased_unwire(&mut unwire_ctx);
        }
        let removed = self.inline.remove(child);
        // Only a slot that was actually ours earns a retirement: the guest
        // registry is the authority on which aliases this cluster resides at,
        // so an idempotent re-despawn (or an alias that named no child) leaves
        // the substrate untouched. wasm32-only — the host build carries no FFI
        // surface, and its inline registry has no substrate route behind it.
        #[cfg(target_family = "wasm")]
        if removed {
            mail::despawn_inline_child(child.0);
        }
        removed
    }
}

/// Resolve a [`Subname`] into the `(is_counter, discriminator)` pair the
/// spawn host fns take, shared by [`WasmCtx::spawn_child`] and
/// [`WasmCtx::spawn_inline_child`]. `Counter` passes an empty discriminator
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

/// Build an inline child's actor value and register it under its alias in
/// `registry` (ADR-0114). Split out of [`WasmCtx::spawn_inline_child`] so
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
/// After the insert, the fresh child's `wire` runs (issue 2746): the child
/// is taken back out of its slot onto the stack, `erased_wire` is driven
/// through a [`WasmCtx`] addressed to its alias, and it is reinserted — the
/// same take/reinsert discipline `membrane_dispatch` uses, so a `wire` that
/// spawns a nested inline child re-enters the registry without aliasing its
/// interior-mutable map. Only the two fresh-spawn paths funnel here; the
/// `replace_component` reconstruct path (`reconstruct_one_child`) has its
/// own insert and runs `init` + `on_rehydrate`, not `wire`, so a reload
/// never fires `wire`.
// The parameters are the slot's reconstruct record (ADR-0114 §5) plus the
// decoded config — a fixed set with no meaningful grouping short of a
// one-use struct.
#[allow(clippy::too_many_arguments)]
pub fn install_inline_child<A>(
    registry: &Registry,
    alias: MailboxId,
    type_tag: u64,
    full_subname: String,
    is_counter: bool,
    parent: u64,
    config_bytes: Vec<u8>,
    config: A::Config,
) -> Result<MailboxId, SpawnError>
where
    A: WasmActor + ErasedWasmActor,
    // iamacoffeepot/aether#2311: `A::init` returns the runtime state, boxed as
    // the erased child. For an un-split component `State = Self`.
    <A as WasmActor>::State: ErasedWasmActor,
{
    let mut ctx = WasmInitCtx::__new(alias.0);
    // ADR-0156 §2: inline children resolve `Params` to the compiled default
    // (empty params for now), mirroring the `()`-config round-trip.
    let params = <A::Params as Default>::default();
    let child = A::init(config, params, &mut ctx).map_err(SpawnError::InitFailed)?;
    registry.insert_child(alias, type_tag, full_subname, is_counter, parent, config_bytes, Box::new(child));
    // Run the fresh child's `wire` (issue 2746). Take it back onto the stack
    // so its slot is empty for the duration — a `wire` that spawns a nested
    // inline child then re-enters the registry (a different slot) with no
    // aliasing, and a `wire` that re-addresses its own alias finds the empty
    // slot, exactly as `membrane_dispatch` handles a resident child. `take`
    // yields `Some` here (the box was just inserted); the `if let` is a
    // defensive no-op rather than an `expect`.
    if let Some(mut fresh) = registry.take(alias) {
        let mut wire_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(alias.0, registry, NO_INBOUND_SOURCE);
        fresh.erased_wire(&mut wire_ctx);
        registry.reinsert(alias, fresh);
    }
    Ok(alias)
}
