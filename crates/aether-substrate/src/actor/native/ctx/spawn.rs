//! Staging a child birth from the handler turn that parents it.
//!
//! The one surface that needs the ctx to name its actor: the parent is read
//! off the ctx rather than declared beside it (issue 4158), so a parent that
//! disagrees with the executing binding has no spelling. Both entry points
//! hand back a [`HandlerSpawnBuilder`] whose only
//! terminals are staged ones — a handler cannot commit a birth itself.
//!
//! Beside them, the staging of a published guest and of a native type
//! spawned by mail (ADR-0241 §9), each placed beneath a proven parent rather
//! than beneath the staging actor.

use std::sync::Arc;

use aether_actor::{CoveredBy, ErasedActorRef, Instanced, Protocol, ProtocolRef, ReplyMode};
use aether_data::{Kind, RequestId};

use crate::actor::native::NativeActor;
use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::actor::native::spawn::by_namespace::NativeBirthSite;
use crate::actor::native::spawn::{
    GuestBirth, GuestSpawnBuilder, HandlerSpawnBuilder, NativeSpawnEntry, SpawnBuilder, SpawnDelivery, SpawnError,
    SpawnReceipt, Subname,
};
#[cfg(feature = "wasm")]
use crate::actor::wasm::kind_manifest::Dependency;
use crate::mail::{Source, SourceAddr};

use super::NativeCtx;

/// The one surface that needs the ctx to name its actor: a birth's parent is
/// the actor being dispatched, so the call exists only where that actor is in
/// scope. An [`Erased`](super::Erased) ctx reaches none of this (issue 4158).
impl<M: ReplyMode, A: NativeActor> NativeCtx<'_, A, M> {
    /// Spawn an instanced `C` as a child of `A`, the actor this ctx
    /// dispatches for. The `C: ChildOf<A>` bound enforces the ADR-0166
    /// permission, and
    /// [`HandlerSpawnBuilder::stage`] /
    /// [`HandlerSpawnBuilder::stage_with`]
    /// prepare the child locally and stage its authoritative owner-time
    /// activation. The new actor's [`Source`]
    /// stamps the calling actor's mailbox so any reply addressed to
    /// `SourceAddr::Component` routes back here.
    ///
    /// The parent is the ctx's own actor, never a caller-supplied one
    /// (issue 4158): the `#[actor]` macro hands every handler whose ctx does
    /// not spell `Erased` — `NativeCtx<'_>`, `NativeCtx<'_, Self, Manual>` —
    /// a ctx typed by the actor it is dispatching for (ADR-0231 §7). A parent that
    /// disagrees with the executing binding is therefore not a runtime
    /// error to check but a state with no spelling.
    ///
    /// Returns a [`HandlerSpawnBuilder`] the
    /// caller chains `after_init` and then `stage`, `stage_with`, or
    /// `continue_from` against. Those staged terminals are the only ones it
    /// has, so a handler cannot commit the birth itself; the authoritative
    /// result arrives later as `TaskDone<SpawnOutcome<C>, _>`, whose `Ok` arm
    /// is the child's proven [`ActorRef<C>`](aether_actor::ActorRef). Eager commit belongs
    /// to the boot/embedder [`SpawnBuilder`] behind
    /// `PassiveChassis::spawn_actor` / `BuiltChassis::spawn_actor`; both
    /// builder shapes flow through the same [`crate::Spawner`].
    ///
    /// # Panics
    /// Panics if the transport is a test binding such as
    /// `testing::unrouted_binding` (which doesn't wire a spawner) —
    /// fail-fast per ADR-0063: production transports always carry one, so
    /// handler code never reaches the panic.
    pub fn spawn_child<'b, C>(
        &'b self,
        subname: Subname<'b>,
        config: C::Config,
        params: C::Params,
    ) -> HandlerSpawnBuilder<'b, C>
    where
        C: aether_actor::ChildOf<A> + Instanced + NativeActor,
    {
        let spawner = self
            .binding
            .spawner()
            .expect("NativeCtx::spawn_child requires a chassis-built binding (no spawner installed — likely a `new_for_test` binding)");
        let sender =
            Source { addr: SourceAddr::Component(self.binding.self_mailbox()), correlation_id: Source::NO_CORRELATION };
        // ADR-0165: the child builder captures the complete typed parent
        // identity so it can derive both lineage and canonical name without
        // a registry lookup.
        let parent = self
            .binding
            .runtime_identity()
            .expect("NativeCtx::spawn_child requires a typed production binding")
            .clone();
        let builder = SpawnBuilder::new_child(Arc::clone(spawner), subname, config, params, sender, parent);
        HandlerSpawnBuilder::new(builder, Arc::clone(self.binding), self.in_flight_root)
    }

    /// Stage the birth of a published guest, hosted by the native `H`, under
    /// the guest's own published name (ADR-0241 §5, §6): `NS`, `NS:key`, or
    /// `parent/NS:key`, as [`GuestBirth`] says. The birth holds no native
    /// namespace; the registry owner admits it only where the publication
    /// table binds `birth.namespace` to `birth.module` (§3), and otherwise
    /// completes it with [`SpawnError::GuestNotPublished`]
    /// and leaves no route.
    ///
    /// The returned [`GuestSpawnBuilder`] stages a task that owes no reply
    /// (ADR-0243 §9). Its completion is `TaskDone<GuestOutcome<P>>`, whose
    /// `Ok` arm is a [`ProtocolRef<P>`](aether_actor::ProtocolRef) over the
    /// rows of `H` the caller controls the guest through: the name is the
    /// guest's, so no `ActorRef<H>` is minted for it.
    ///
    /// A parented birth's parent is the proof `birth.parent` carries, and its
    /// lineage is read off that proof, so the lineage the guest extends cannot
    /// disagree with the position it is born under. The staging actor holds
    /// the parent-local key.
    ///
    /// # Panics
    /// Panics if the transport carries no spawner, as [`Self::spawn_child`]
    /// does.
    pub fn spawn_guest<'b, H, P>(
        &'b self,
        birth: GuestBirth<'b>,
        config: H::Config,
        params: H::Params,
    ) -> GuestSpawnBuilder<'b, H, P>
    where
        H: Instanced + NativeActor,
        P: Protocol + CoveredBy<H> + 'static,
    {
        let spawner = self.binding.spawner().expect("NativeCtx::spawn_guest requires a chassis-built binding");
        let parent = birth.parent.map(|parent| self.scoped_parent(parent));
        GuestSpawnBuilder::new(
            Arc::clone(spawner),
            birth,
            parent,
            config,
            params,
            Arc::clone(self.binding),
            self.in_flight_root,
        )
    }

    /// Stage the birth of the mail-spawnable native type `entry` names
    /// (ADR-0241 §9), keyed by `key`, at the root or beneath the live
    /// `parent`, with `context` stored for its completion (ADR-0243 §9). The
    /// type's `Config` resolves over the engine's config source stack, and
    /// its `Params` is `()`. The birth runs the ordinary owner commit and
    /// publication-table hold: a tombstoned name completes with
    /// [`SpawnError::SubnameRetired`], and the completion is
    /// `TaskDone<NativeSpawnOutcome>`, whose `Ok` arm is the born actor's
    /// [`SpawnDelivery`] proof. The caller checks the type's placement
    /// ([`NativeSpawnEntry::declares_root`],
    /// [`NativeSpawnEntry::declares_child_of`]) first; a parented birth's
    /// parent-local key is held by this actor, as a guest birth's is.
    ///
    /// # Errors
    ///
    /// The [`SpawnError`] of the first synchronous step that refuses the
    /// birth, with `context` handed back unstored: a type
    /// [`NativeSpawnEntry::stageable`] refuses
    /// ([`SpawnError::NotSpawnableByMail`]), a config that fails to resolve, an
    /// invalid key, a path over the scope caps, a name this actor already
    /// holds beneath `parent`, or a failed build.
    ///
    /// # Panics
    /// Panics if the transport carries no spawner, as [`Self::spawn_child`]
    /// does.
    pub fn spawn_native<C: Kind>(
        &self,
        entry: &NativeSpawnEntry,
        key: Subname<'_>,
        parent: Option<ErasedActorRef>,
        context: C,
    ) -> Result<SpawnReceipt, (SpawnError, C)> {
        let Some(prepare) = entry.stager() else {
            return Err((SpawnError::NotSpawnableByMail { namespace: entry.namespace() }, context));
        };
        let spawner = self.binding.spawner().expect("NativeCtx::spawn_native requires a chassis-built binding");
        let site = NativeBirthSite {
            spawner: Arc::clone(spawner),
            binding: Arc::clone(self.binding),
            completion_root: self.in_flight_root,
            parent: parent.map(|parent| self.scoped_parent(parent)),
            key,
        };
        let birth = match prepare(site) {
            Ok(birth) => birth,
            Err(error) => return Err((error, context)),
        };

        let request = RequestId(self.binding.mint_correlation());
        self.binding.store_request_context(request, context);
        Ok(birth.stage_as_task(request))
    }

    /// The live instance of the mail-spawnable native type `entry` names at
    /// the name a spawn keyed by `key` beneath `parent` would take, as its
    /// [`SpawnDelivery`] proof (ADR-0241 §9), or `None` when no `Live` route
    /// stands there, a tombstoned name included. A singleton is found at its
    /// namespace, keyless and unparented; an instanced type at `NS:key` or
    /// `parent/NS:key`.
    #[must_use]
    pub fn live_native(
        &self,
        entry: &NativeSpawnEntry,
        key: Option<&str>,
        parent: Option<ErasedActorRef>,
    ) -> Option<ProtocolRef<SpawnDelivery>> {
        entry.live(self.binding.mailer().registry(), key, parent)
    }

    /// The runtime identity a live `parent` proves, for a birth placed
    /// beneath it rather than beneath this ctx's actor. Its mailbox id is its
    /// lineage carry: a tag rewrites only bits no later fold reads.
    fn scoped_parent(&self, parent: ErasedActorRef) -> ActorRuntimeIdentity {
        ActorRuntimeIdentity::new(parent.id(), None, parent.id().0, self.actor_path(parent))
    }
}

/// The dependency read: every declarable dependency is a root singleton, so
/// it takes no placement and the erased ctx reaches it too.
impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// The namespace of the first declared dependency with no `Live` route,
    /// or `None` when every entry is live. Every entry is a root singleton
    /// (ADR-0241 §5), so its `One` entry folds from the root wherever the
    /// dependent is placed. It is a read and nothing else: no ordering, no
    /// retry, no wait.
    ///
    /// Its consumers are the component host's loads, module boots, and
    /// republish pre-checks; the inline spawn host fn asks the same read
    /// through the guest's binding.
    #[cfg(feature = "wasm")]
    #[must_use]
    pub fn missing_dependency<'d>(&self, dependencies: &'d [Dependency]) -> Option<&'d str> {
        self.binding.missing_dependency(dependencies.iter().map(|d| (d.resolver, d.namespace.as_str())))
    }
}
