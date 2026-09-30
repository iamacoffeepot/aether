//! The registry's liveness reads, [`Registry::is_live`] over a reference and
//! the crate-private position form beside it, and the eight mints beside
//! them.
//!
//! The callers of the gated mint outside the SDK itself. Two mint with no
//! read, and both are typed: the `Registry::declared_dependency` caller
//! proved the dependency `Live` at the dependent's birth, and every
//! `Registry::activated` caller has just published the actor's own `Live`
//! route, whether the birth was a staged child, an embedder spawn, or a
//! chassis-composed capability. The third, `Registry::stamped_sender`, mints
//! a host-stamped position — a dispatch source, or the sender half of a
//! reply's mail id — only once one published-route read finds a record
//! standing there, so every erased reference names a route. The fourth,
//! `Registry::resolve_live`, is the only one that answers the liveness
//! question itself, because the position it is handed arrived in a payload
//! and nothing upstream proved it. The fifth, `Registry::loaded`, types a
//! reference the caller already holds, and the sixth, `Registry::live_child`,
//! answers the same liveness question for a child key folded beneath a
//! parent the caller already proved. The seventh, `Registry::resolve_protocol`,
//! proves a protocol path that arrived in mail: it answers the same liveness
//! question at the path's canonical name. The eighth, `Registry::cast`, types
//! an erased reference the caller already holds as a protocol, once one read
//! of the published view finds its route `Live` and publishing rows the
//! protocol admits.
//!
//! One read beside them mints nothing: `Registry::published_rows_at` answers
//! the rows a `Live` route published, the row source the native cast and
//! the wasm guest's cast both read.

use core::fmt;
use std::error::Error;
use std::sync::Arc;

use aether_actor::{
    __mint_actor_ref, __mint_erased_actor_ref, __mint_protocol_ref, ActorRef, CastTarget, ErasedActorRef, Instanced,
    Protocol, ProtocolPath, ProtocolRef, ResolveError,
};
use aether_data::{LoadName, MailboxCategory, ReplyContract};

use crate::mail::registry::RouteContract;
use crate::mail::{KindId, MailboxId};

use super::resolve::{ResolvedRoute, resolve_route};
use super::{CapturedDisposition, Registry};

/// Why an embedder could not type a load reply's sender as the loaded actor
/// (`PassiveChassis::adopt_load`). Carries no position: the embedder already
/// holds the erased reference it asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptRefused {
    /// The sender's route is not `Live` now: the component was dropped
    /// before the embedder adopted its reply.
    NotLive,
    /// The sender is live but is not a loaded component's trampoline, so the
    /// reply did not come from a load.
    NotComponent,
}

impl fmt::Display for AdoptRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLive => formatter.write_str("the load reply's sender is no longer live"),
            Self::NotComponent => formatter.write_str("the load reply's sender is not a loaded component"),
        }
    }
}

impl Error for AdoptRefused {}

/// Why no live child stands at a key beneath a proven parent
/// (`PassiveChassis::child`). Names the child by its key and actor namespace,
/// never by a position: the embedder asked with a parent reference and a key,
/// and those are what it can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRefused {
    /// The child's `NAMESPACE`.
    pub namespace: &'static str,
    /// The instance key the embedder asked for.
    pub key: LoadName,
}

impl fmt::Display for ChildRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "no live {} child keyed {:?} beneath the parent", self.namespace, self.key.as_str())
    }
}

impl Error for ChildRefused {}

/// Why a position that arrived in a payload could not be proven
/// (ADR-0230 section 3).
///
/// The two arms are the distinction a refusing capability reports: a
/// `Dropped` route names an actor that existed and has since retired, while
/// an `Unknown` one names an id nothing ever registered — a fold the caller
/// computed, or a typo. A cap that collapsed them would tell a component
/// that unloaded cleanly the same thing it tells a caller who guessed an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveLiveError {
    /// A route was published under this id and has since been dropped.
    Dropped(MailboxId),
    /// No live route stands under this id: it was never registered, its
    /// unborn claim was withdrawn, or its birth is still `Starting` and so
    /// not yet provable.
    Unknown(MailboxId),
}

impl fmt::Display for ResolveLiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dropped(id) => write!(formatter, "mailbox {id:?} already dropped"),
            Self::Unknown(id) => write!(formatter, "unknown mailbox id {id:?}"),
        }
    }
}

impl Registry {
    /// Whether the actor `target` proves is still `Live` in the published
    /// route view. A reference proves only that its actor reached `Live`
    /// (ADR-0230), which stays true after it departs; this answers the other
    /// question, "is it `Live` now".
    ///
    /// [`ActorProbe`](crate::actor::native::ActorProbe) is the consumer, on
    /// behalf of the http server's request reader: it holds route members as
    /// references and skips one whose actor has departed but whose
    /// `MonitorNotice` has not yet purged it.
    pub fn is_live(&self, target: ErasedActorRef) -> bool {
        self.is_live_at(target.id())
    }

    /// Whether the published route view holds a `Live` endpoint at
    /// `candidate` — `Starting`, `Dropped`, and `Unknown` alike mean
    /// "not live".
    ///
    /// Reads the lock-free published snapshot through the hot-path
    /// `route_lookup`, exactly what the mailer's route step reads — no
    /// mail, no allocation, no lock the send path does not take.
    pub(crate) fn is_live_at(&self, candidate: MailboxId) -> bool {
        // `route_lookup` ignores its kind on this path (the mailer's route
        // step passes the live kind); the zero kind carries that.
        matches!(self.route_lookup(KindId(0), candidate).into_captured(), CapturedDisposition::Live { .. })
    }

    /// Mint a reference for a declared dependency's `position`, with no
    /// registry read (ADR-0230).
    ///
    /// The one caller,
    /// [`NativeCtx::actor_ref`](crate::actor::native::NativeCtx::actor_ref),
    /// discharges the obligation `A: DependsOn<R>`: the dependent's birth was
    /// refused unless `R` was `Live`, checked before `init`, so the answer is
    /// already known. It performs no read because the claim an [`ActorRef`]
    /// carries is "reached `Live`", not "is `Live` now" — a `Dropped`
    /// dependency still reached `Live`, and [`Self::is_live_at`] would answer
    /// `false` for it.
    pub(crate) fn declared_dependency<R>(position: MailboxId) -> ActorRef<R> {
        __mint_actor_ref(position)
    }

    /// Mint a reference for an actor whose `Live` route the caller has just
    /// published at `position`, with no registry read (ADR-0230 section 3's
    /// spawned-or-loaded door).
    ///
    /// The one claim every caller discharges is that it has itself just made
    /// this actor `Live`, so a read would repeat what the caller just decided —
    /// exactly as for [`Self::declared_dependency`]. The callers:
    ///
    /// - the native spawn finalizer's `promote`, which runs inside the catch-up
    ///   suffix the registry owner calls only after it has published a staged
    ///   child's `Live` route;
    /// - the eager [`SpawnBuilder::finish`](crate::SpawnBuilder::finish) terminals,
    ///   whose commit returns only once the route is `Live` — written directly
    ///   before the ADR-0165 seal, promoted by the owner after it;
    /// - the chassis boot of a composed capability, which records the reference
    ///   once the boot claim published the route and `init` and `wire` succeeded;
    /// - both pumped-actor boots, after the route was published `Live`.
    ///
    /// A refused birth never reaches it: it completes with its
    /// [`SpawnError`](crate::actor::native::SpawnError) or [`BootError`](crate::BootError)
    /// and mints nothing.
    pub(crate) fn activated<A>(position: MailboxId) -> ActorRef<A> {
        __mint_actor_ref(position)
    }

    /// Mint an erased reference for the host-stamped `position` when a route
    /// record stands there, and `None` when none does (ADR-0230).
    ///
    /// The position is a stamp the host wrote — the dispatch source, or the
    /// sender half of the mail id a replier minted in its own id space — but
    /// `Mail`, `Source`, and the mailer's push and reply entries are public,
    /// so a stamp alone does not show the position was ever registered. The
    /// one read settles it: a reference minted here names a record
    /// [`Self::actor_path`] reads through the same view, and a record leaves
    /// the table only when a `Starting` reservation is cancelled, or a claim
    /// is withdrawn before any actor could have observed it — never after it
    /// has emitted mail — so its path answers for the session.
    ///
    /// Every lifecycle counts. A `Dropped` or retired-alias record is the
    /// departed actor a [`MonitorNotice`](aether_kinds::MonitorNotice) is
    /// stamped with, and a `Starting` record is a post-seal pumped actor whose
    /// `wire` mail leaves before its route is promoted. The chassis sentinel
    /// answers `None` with no special case, because no publish arm ever
    /// records a route at it.
    ///
    /// The cost is one lock-free load of the published route view and one
    /// hash probe — the read `route_lookup` already takes on every send.
    ///
    /// Its callers are
    /// [`NativeCtx::sender`](crate::actor::native::NativeCtx::sender), for
    /// both a component source and a reply's replier, and the session arms
    /// of `Mailer::send_reply` and the wasm guest's `reply_mail_p32`, which
    /// stamp the replying actor on an egressed reply event.
    pub(crate) fn stamped_sender(&self, position: MailboxId) -> Option<ErasedActorRef> {
        self.routes.load().entry_for(&position).map(|_| __mint_erased_actor_ref(position))
    }

    /// Type the stamped sender of a load reply as the loaded actor `R`
    /// (ADR-0230 section 3's spawned-or-loaded door, for an embedder).
    ///
    /// A successful load reply is sent by the loaded actor itself, so the
    /// embedder already holds its erased reference from the reply event's
    /// stamped sender; this narrows it after checking the claim the typing
    /// adds: the sender's route is `Live` and is a guest, one whose namespace
    /// a published module implements, the only actor the component host hands
    /// a load to. `R` is the export the
    /// embedder named in its load, which the host instantiated.
    ///
    /// Its one caller is
    /// [`PassiveChassis::adopt_load`](crate::chassis::builder::PassiveChassis::adopt_load),
    /// through the spawner that holds the chassis registry.
    pub(crate) fn loaded<R>(&self, sender: ErasedActorRef) -> Result<ActorRef<R>, AdoptRefused> {
        let position = sender.id();
        if !self.is_live_at(position) {
            return Err(AdoptRefused::NotLive);
        }
        // The category the inventory publication gave the route, read from
        // the publication table rather than the name (ADR-0241 §3).
        let inventory = self.inventory.load();
        let category = inventory.table().mailboxes.iter().find(|entry| entry.id == position).and_then(|d| d.category);
        if category != Some(MailboxCategory::Trampoline) {
            return Err(AdoptRefused::NotComponent);
        }
        Ok(__mint_actor_ref(position))
    }

    /// Prove a `position` that arrived in a payload (ADR-0230 section 3's
    /// payload-borne-id door), or say why it cannot be proven.
    ///
    /// The module's one mint that answers the liveness question. The typed
    /// mints discharge an obligation something upstream already answered — a
    /// refused birth, a published route — and [`Self::stamped_sender`] asks
    /// only whether any record stands at a host stamp, whereas a position
    /// carried in a kind field is a position and nothing more, so the only
    /// authority on whether it is occupied is this view. The read is the same published-route walk
    /// [`Self::entry_at`] takes, so a proof and a dispatch agree by construction
    /// rather than by two lookups kept in step.
    ///
    /// `Starting` reads as [`ResolveLiveError::Unknown`]: ADR-0230 section 1
    /// keeps `Starting` internal to the registry, so it is never a state a
    /// reference may be issued against — a reservation whose `init` fails is
    /// removed, and a reference minted against it would have outlived its
    /// claim.
    ///
    /// An inline-child alias mints, because `resolve_route`'s alias arm
    /// resolves to the host's `Live` endpoint: the alias names a real actor
    /// that reached `Live`, which is exactly what the reference claims. That
    /// is ADR-0230's closing consequence — the proof is about the actor the
    /// position reaches, not about the shape of the route record.
    ///
    /// Its callers are
    /// [`NativeCtx::resolve_live`](crate::actor::native::NativeCtx::resolve_live),
    /// the single public spelling a capability uses, and the test-support
    /// fixtures `testing::registered_ref` and `testing::registered_binding`,
    /// which prove the inbox they just registered.
    pub(crate) fn resolve_live(&self, position: MailboxId) -> Result<ErasedActorRef, ResolveLiveError> {
        let routes = self.routes.load();
        match resolve_route(position, |candidate| routes.entry_for(&candidate)) {
            ResolvedRoute::Live { .. } => Ok(__mint_erased_actor_ref(position)),
            ResolvedRoute::Dropped { .. } => Err(ResolveLiveError::Dropped(position)),
            ResolvedRoute::Starting { .. } | ResolvedRoute::Unknown => Err(ResolveLiveError::Unknown(position)),
        }
    }

    /// Prove the child of type `C` at `key` beneath a parent the caller
    /// already proved (ADR-0230 section 3's child-beneath-a-held-reference
    /// door, for an embedder).
    ///
    /// The key is folded with `C`'s resolver beneath the parent's position,
    /// and the folded position is proven with the same published-route walk
    /// [`Self::resolve_live`] takes: only a `Live` route mints. `Starting`,
    /// `Dropped`, and never-registered positions refuse alike. The parent's
    /// proof anchors the fold, and the key is all it takes.
    ///
    /// Its one caller is
    /// [`PassiveChassis::child`](crate::chassis::builder::PassiveChassis::child),
    /// through the spawner that holds the chassis registry; it passes the
    /// parent's reference erased, and keeps the `ChildOf` placement bound on
    /// its own signature.
    pub(crate) fn live_child<C: Instanced>(
        &self,
        parent: ErasedActorRef,
        key: LoadName,
    ) -> Result<ActorRef<C>, ChildRefused> {
        let position = C::resolve(parent.id().0, key.as_str());

        let routes = self.routes.load();
        match resolve_route(position, |candidate| routes.entry_for(&candidate)) {
            ResolvedRoute::Live { .. } => Ok(__mint_actor_ref(position)),
            ResolvedRoute::Dropped { .. } | ResolvedRoute::Starting { .. } | ResolvedRoute::Unknown => {
                Err(ChildRefused { namespace: C::NAMESPACE, key })
            }
        }
    }

    /// Prove a protocol path that arrived in mail (ADR-0231 §3's receipt of
    /// a `ProtocolPath<P>`).
    ///
    /// The path is folded as written and one read of the published view,
    /// [`Self::live_route`], finds the `Live` route under exactly that
    /// canonical name, or refuses [`ResolveError::NotLive`] naming the path.
    /// The reference proves liveness only, and `P` is the path's claim.
    ///
    /// Its one caller is
    /// [`NativeCtx::resolve`](crate::actor::native::NativeCtx::resolve).
    pub(crate) fn resolve_protocol<P: Protocol>(&self, path: &ProtocolPath<P>) -> Result<ProtocolRef<P>, ResolveError> {
        let path = path.as_erased();
        self.live_route(path).map(__mint_protocol_ref).ok_or_else(|| ResolveError::NotLive { path: path.clone() })
    }

    /// The rows the route at `position` published, answered only while it
    /// resolves `Live` (ADR-0231 §4): the one row source both guard casts
    /// read.
    ///
    /// One read of the published view, [`Self::published_contract`], finds
    /// the route `Live` and reads its rows. A `Starting`, `Dropped`, or
    /// unknown route answers `None`; a closure route answers its empty
    /// contract. The rows are the facts `describe_component` already
    /// exposes, and the read mints nothing, so a position that arrived from
    /// a guest learns nothing new and gets no reference from here.
    ///
    /// Its callers are [`Self::cast`], the native guard cast, and
    /// `NativeBinding::published_rows_at`, the read behind the wasm guest's
    /// `published_rows_p32` host fn, whose guest applies the same
    /// `CastTarget::admits` rule and mints its own reference.
    pub(crate) fn published_rows_at(&self, position: MailboxId) -> Option<Arc<[(KindId, ReplyContract)]>> {
        self.published_contract(position).map(RouteContract::into_rows)
    }

    /// Type an erased reference the caller already holds as the protocol `T`
    /// (ADR-0231 §4's guard cast), or answer `None`.
    ///
    /// [`Self::published_rows_at`] reads the rows the route `reference` proves
    /// published while it is `Live`; `T::admits` decides whether those rows
    /// answer `T`: the subscriber arm's silent-or-unchecked rule or the protocol
    /// arm's exact-rows rule (ADR-0231 §4), both fixed in `aether-actor`. A
    /// `Starting`, `Dropped`, or unknown route answers `None`, as does a live
    /// one whose rows `T` does not admit, such as a closure route's empty
    /// contract.
    ///
    /// Its callers are
    /// [`NativeCtx::cast`](crate::actor::native::NativeCtx::cast) and
    /// [`PassiveChassis::cast`](crate::PassiveChassis::cast).
    pub(crate) fn cast<T: CastTarget>(&self, reference: ErasedActorRef) -> Option<ProtocolRef<T>> {
        let position = reference.id();

        T::admits(&self.published_rows_at(position)?).then(|| __mint_protocol_ref(position))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use aether_actor::{Addressable, Many, Resolve};

    use crate::config::RegistryQueueCapacities;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::effect::{EffectBatch, RegistryApplied, RegistryEffect};
    use crate::mail::registry::owner::RegistryOwnerLease;
    use crate::mail::registry::{MailDispatch, noop_handler};
    use crate::scheduler::WakeSink;
    use crate::testing::boot_authority;

    use super::*;

    struct ProbeChild;

    impl Addressable for ProbeChild {
        const NAMESPACE: &'static str = "test.proven.child";
        type Resolver = Many;
    }

    fn child_key(key: &str) -> LoadName {
        LoadName::new(key).expect("a valid key")
    }

    // A child lookup that answered for any folded position would hand an
    // embedder a reference to a child that never reached `Live`, or to a
    // sibling under a different key. It proves only the `Live` route at the
    // exact fold beneath the parent, and refuses a `Starting` birth.
    #[test]
    fn live_child_proves_only_a_live_route_at_the_keyed_fold() {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let authority = boot_authority();
        let owner = RegistryOwnerLease::attach(
            boot_authority(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );
        let parent = registry.register_inbox(&authority, "test.proven.parent", noop_handler());
        let live = Many::resolve(parent.0, ProbeChild::NAMESPACE, "live");
        registry
            .try_register_inbox_with_id(&authority, live, "test.proven.parent/test.proven.child:live", noop_handler())
            .expect("register the live child");
        let starting = Many::resolve(parent.0, ProbeChild::NAMESPACE, "starting");
        let completion = registry
            .submit(EffectBatch::new(vec![RegistryEffect::reserve_with_id(
                starting,
                "test.proven.parent/test.proven.child:starting".to_owned(),
            )]))
            .expect("owner accepts the Starting reservation");
        owner.run_once();
        let reserved = completion.wait_timeout(Duration::from_millis(100)).expect("reservation completes");
        assert!(matches!(reserved.as_deref(), Ok([RegistryApplied::Starting { .. }])));

        let parent_ref = registry.resolve_live(parent).expect("the parent route is live");

        assert!(registry.live_child::<ProbeChild>(parent_ref, child_key("live")).is_ok());
        let refused =
            registry.live_child::<ProbeChild>(parent_ref, child_key("starting")).expect_err("Starting is not provable");
        assert_eq!(refused.key.as_str(), "starting");
        assert_eq!(refused.namespace, ProbeChild::NAMESPACE);
        assert!(
            registry.live_child::<ProbeChild>(parent_ref, child_key("other")).is_err(),
            "a wrong key names no child"
        );
    }

    #[test]
    fn is_live_is_true_only_for_live_routes() {
        let registry = Registry::new();
        let authority = boot_authority();
        let live = registry.register_inbox(&authority, "test.proven.live", noop_handler());
        let dropped = registry.register_inbox(&authority, "test.proven.dropped", noop_handler());
        assert!(registry.drop_mailbox(&authority, dropped).is_ok());

        assert!(registry.is_live_at(live));
        assert!(!registry.is_live_at(dropped), "a Dropped route is not live");
        assert!(!registry.is_live_at(MailboxId(0xdead_beef)), "an unknown id is not live");
    }

    // Tripwire: the accept set `resolve_live` mints over includes inline
    // routes, and its refusal splits dropped from unknown. The window cap's
    // subscriber accept set is inline and inline-alias routes as much as it
    // is inboxes, so a `resolve_live` rewritten over `entry() == Inbox { .. }`
    // or over the actor-registry slot check would silently stop every inline
    // subscriber from subscribing; and both refusal texts every cap reports
    // are read off the two arms this pins apart.
    #[test]
    fn resolve_live_proves_an_inline_route_and_splits_its_two_refusals() {
        let registry = Registry::new();
        let authority = boot_authority();
        let inline =
            registry.register_inline(&authority, "test.proven.inline", Arc::new(|_: MailDispatch<'_>| {})).id();

        assert!(registry.resolve_live(inline).is_ok(), "an inline-handler mailbox is provable");

        assert!(registry.drop_mailbox(&authority, inline).is_ok());
        assert_eq!(registry.resolve_live(inline), Err(ResolveLiveError::Dropped(inline)));
        assert_eq!(
            registry.resolve_live(MailboxId(0xdead_beef)),
            Err(ResolveLiveError::Unknown(MailboxId(0xdead_beef))),
        );
    }
}
