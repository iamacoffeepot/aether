//! The eager builder for the boot / embedder boundary.
//!
//! Its terminals write registry, liveness, cost, and slot state
//! synchronously, which is correct only before the ADR-0165 owner seal —
//! and after it, the same call goes through the owner and blocks on its
//! decision so `finish` keeps the read-your-writes contract its callers
//! have always had. Both constructors are crate-internal, so the only
//! builders that reach outside the substrate come from the chassis entry
//! points; handler code holds a [`HandlerSpawnBuilder`](super::HandlerSpawnBuilder)
//! instead.

use std::marker::PhantomData;
use std::sync::Arc;

use aether_actor::{HandlesKind, Instanced, validate_namespace_segment};
use aether_data::Kind;
use aether_kinds::trace::Nanos;

use crate::actor::native::NativeActor;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::mail::{KindId, MailId, MailRef, MailboxId, Source};

use super::spawner::Spawner;
use super::spawner::commit::SpawnCommit;
use super::{SpawnError, Subname};

/// Eager builder for the boot/embedder boundary, returned from
/// `BuiltChassis::spawn_actor` and `PassiveChassis::spawn_actor` (and the
/// `cfg`-gated `spawn_actor_for_test`), and wrapped by
/// [`HandlerSpawnBuilder`](super::HandlerSpawnBuilder) for handler-local child staging. Lets the caller
/// chain `after_init` to pre-load bootstrap mail before its terminal
/// operation.
///
/// Holds the spawner reference borrowed from the calling ctx's
/// transport, the resolved subname, the consumed config, and the
/// running list of after-init envelopes. `finish` consumes the
/// builder and runs the spawn lifecycle.
///
/// Its [`finish`](Self::finish) / [`finish_with_name`](Self::finish_with_name)
/// terminals write the registry, liveness, cost, and slot state synchronously,
/// which is correct only before the ADR-0165 owner seal. Both constructors are
/// crate-internal, so the only builders that reach outside the substrate come
/// from those chassis entry points; handler code holds a
/// [`HandlerSpawnBuilder`] instead and can reach nothing but the staged
/// terminals.
pub struct SpawnBuilder<'ctx, A: Instanced + NativeActor> {
    pub(in crate::actor::native::spawn) spawner: Arc<Spawner>,
    pub(in crate::actor::native::spawn) subname: Subname<'ctx>,
    pub(in crate::actor::native::spawn) config: Option<A::Config>,
    /// ADR-0156 §2 composer-supplied params, threaded to `A::init` beside
    /// `config`. Taken with `config` when `finish` runs.
    pub(in crate::actor::native::spawn) params: Option<A::Params>,
    pub(in crate::actor::native::spawn) sender: Source,
    /// ADR-0165: the spawning actor's typed runtime identity, or
    /// `None` for a top-level chassis-level spawn. `Some` nests the
    /// child — its id folds the new node's `ActorId` onto the parent
    /// carry, and its registered name renders under the parent's. `None`
    /// is the depth-1 case: the child is the root of its own lineage and
    /// keeps the flat `{NAMESPACE}:{subname}` id it has today.
    pub(in crate::actor::native::spawn) parent: Option<ActorRuntimeIdentity>,
    pub(in crate::actor::native::spawn) after_init: Vec<Envelope>,
    _marker: PhantomData<fn() -> A>,
    /// Carries the `'ctx` lifetime even though `spawner` is `Arc`
    /// (no longer borrowed). The lifetime ties `Subname::Named(&str)`
    /// to whatever borrow it was constructed from at the call site,
    /// so a stack-local subname doesn't dangle past `finish()`.
    _ctx: PhantomData<&'ctx ()>,
}
impl<'ctx, A: Instanced + NativeActor> SpawnBuilder<'ctx, A> {
    /// Internal constructor for the top-level chassis spawn. Public only
    /// because chassis-level `spawn_actor` entry points (on `BuiltChassis` /
    /// `PassiveChassis`) build these too.
    ///
    /// It takes no parent: this is the depth-1 placement, so the child is the
    /// root of its own lineage and keeps the flat `{NAMESPACE}:{subname}` id
    /// (ADR-0099 §3). A birth under a parent goes through
    /// [`Self::new_child`] instead (issue 4135).
    pub(crate) fn new(
        spawner: Arc<Spawner>,
        subname: Subname<'ctx>,
        config: A::Config,
        params: A::Params,
        sender: Source,
    ) -> Self {
        Self {
            spawner,
            subname,
            config: Some(config),
            params: Some(params),
            sender,
            parent: None,
            after_init: Vec::new(),
            _marker: PhantomData,
            _ctx: PhantomData,
        }
    }

    /// Internal constructor for a birth under a parent actor.
    ///
    /// `parent` is the spawning actor's own runtime identity, read off the
    /// staging ctx — never a caller-declared type — so there is nothing here
    /// to disagree with the executing binding (issue 4158).
    pub(crate) fn new_child(
        spawner: Arc<Spawner>,
        subname: Subname<'ctx>,
        config: A::Config,
        params: A::Params,
        sender: Source,
        parent: ActorRuntimeIdentity,
    ) -> Self {
        Self {
            spawner,
            subname,
            config: Some(config),
            params: Some(params),
            sender,
            parent: Some(parent),
            after_init: Vec::new(),
            _marker: PhantomData,
            _ctx: PhantomData,
        }
    }

    /// Append `mail` to the bootstrap sequence. Order-preserving —
    /// the spawned actor sees envelopes in the order they were added.
    /// Sender on each envelope is the spawner's reply target; `reply_to`
    /// defaults to the spawner's mailbox.
    ///
    /// `A: HandlesKind<K>` ensures only kinds the actor's handler set
    /// covers can be pre-loaded; the strict-receiver miss path stays
    /// off the bootstrap surface.
    // `mail` is taken by value so the builder API mirrors the rest of
    // the spawn surface (`config: A::Config` is also by value); the
    // value flows straight into `encode_into_bytes` whose owned form
    // matches `Kind`'s wire-encoding convention.
    #[allow(clippy::needless_pass_by_value)]
    #[must_use]
    pub fn after_init<K>(mut self, mail: K) -> Self
    where
        A: HandlesKind<K>,
        K: Kind,
    {
        let payload = mail.encode_into_bytes();
        let kind = KindId(<K as Kind>::ID.0);
        // ADR-0094: the bootstrap seed carries no settlement lineage
        // (`MailId::NONE`), so it is built *disarmed* — there is no
        // obligation to discharge (and `dispatch_one` no-ops its
        // `record_finished` on `NONE` anyway).
        let env = Envelope::disarmed(
            kind,
            None,
            self.sender,
            MailRef::from(payload),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            // Bootstrap seed carries no lineage (`MailId::NONE`), so it
            // never folds into a traced tree node — no deposit instant to
            // record (iamacoffeepot/aether#1134).
            Nanos(0),
            0,
            MailboxId(0),
        );
        self.after_init.push(env);
        self
    }

    /// Consume the builder through the one shared terminal path.
    fn finish_internal(self) -> Result<SpawnCommit, SpawnError> {
        let SpawnBuilder { spawner, subname, config, params, sender, parent, after_init, .. } = self;
        let config = config.expect("SpawnBuilder::finish consumed exactly once");
        let params = params.expect("SpawnBuilder::finish consumed exactly once");
        if let Subname::Named(subname) = subname {
            validate_namespace_segment(subname).map_err(SpawnError::SubnameInvalid)?;
        }
        let _ = sender;
        let identity = spawner.preflight::<A>(subname, parent.as_ref())?;
        let staged = spawner.build::<A>(identity, config, params, after_init)?;
        spawner.commit(staged)
    }

    /// Consume the builder and run the spawn lifecycle. Returns the
    /// new actor's [`MailboxId`] on success, or a typed [`SpawnError`]
    /// describing which lifecycle step failed.
    ///
    /// Boot/embedder authority: the commit half writes shared registry and
    /// scheduler state on the calling thread, which the ADR-0165 owner seal
    /// permits only before the owner takes over. A handler stages instead —
    /// see [`HandlerSpawnBuilder::stage`](super::HandlerSpawnBuilder::stage).
    pub fn finish(self) -> Result<MailboxId, SpawnError> {
        self.finish_internal().map(|commit| commit.mailbox_id)
    }

    /// Consume the builder and return both the mailbox id and the exact
    /// canonical name registered for the new actor. Carries the same
    /// boot/embedder authority as [`Self::finish`].
    pub fn finish_with_name(self) -> Result<(MailboxId, String), SpawnError> {
        self.finish_internal().map(|commit| (commit.mailbox_id, commit.canonical_name))
    }
}
