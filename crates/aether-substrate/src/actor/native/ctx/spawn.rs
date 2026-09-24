//! Staging a child birth from the handler turn that parents it.
//!
//! The one surface that needs the ctx to name its actor: the parent is read
//! off the ctx rather than declared beside it (issue 4158), so a parent that
//! disagrees with the executing binding has no spelling. Both entry points
//! hand back a [`HandlerSpawnBuilder`] whose only
//! terminals are staged ones — a handler cannot commit a birth itself.

use std::sync::Arc;

use aether_actor::{Instanced, ReplyMode};
use aether_data::MailboxId;

use crate::actor::native::NativeActor;
use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::actor::native::spawn::{HandlerSpawnBuilder, SpawnBuilder, Subname};
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
        HandlerSpawnBuilder::new(builder, Arc::clone(self.binding), self.in_flight_root, self.reply_target())
    }

    /// Stage a child under an already-validated logical actor identity that
    /// shares this ctx's physical binding. This is the wasm trampoline seam:
    /// an inline actor executes inside the root trampoline but its detached
    /// child must extend the inline actor's lineage. The component host
    /// validates `parent` against its active cluster and supplies the
    /// registry-owned canonical `parent_name`; native actor code should use
    /// [`Self::spawn_child`] instead.
    #[doc(hidden)]
    pub fn spawn_child_scoped<'b, C>(
        &'b self,
        parent: MailboxId,
        parent_name: Arc<str>,
        subname: Subname<'b>,
        config: C::Config,
        params: C::Params,
    ) -> HandlerSpawnBuilder<'b, C>
    where
        C: aether_actor::ChildOf<A> + Instanced + NativeActor,
    {
        let spawner = self.binding.spawner().expect("NativeCtx::spawn_child_scoped requires a chassis-built binding");
        let sender =
            Source { addr: SourceAddr::Component(self.binding.self_mailbox()), correlation_id: Source::NO_CORRELATION };
        let parent = ActorRuntimeIdentity::new(parent, None, parent.0, parent_name);
        let builder = SpawnBuilder::new_child(Arc::clone(spawner), subname, config, params, sender, parent);
        HandlerSpawnBuilder::new(builder, Arc::clone(self.binding), self.in_flight_root, self.reply_target())
    }

    /// The namespace of the first declared dependency with no `Live` route
    /// for a child placed under the calling actor, or `None` when every entry
    /// is live: [`Registry::missing_dependency`](crate::mail::registry::Registry::missing_dependency)
    /// with this ctx's actor as the placement parent. A `One` entry folds from
    /// the root and an `Embedded` entry folds beneath this actor. It is a read
    /// and nothing else: no ordering, no retry, no wait.
    ///
    /// The dependencies are a wasm module's, which arrive per module; a
    /// native birth checks its own at spawn. Like [`Self::spawn_child`] the
    /// parent is the ctx's own actor, so the erased ctx has no spelling of it.
    /// Its consumers are the component host's module boot, which spawns the
    /// boot trampoline under the host, and its host-placed load.
    #[cfg(feature = "wasm")]
    #[must_use]
    pub fn missing_child_dependency<'d>(&self, dependencies: &'d [Dependency]) -> Option<&'d str> {
        self.binding.missing_child_dependency(dependencies.iter().map(|d| (d.resolver, d.namespace.as_str())))
    }
}
